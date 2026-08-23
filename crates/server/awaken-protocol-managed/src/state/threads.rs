//! Session thread projections for [`ManagedState`]: the primary thread and
//! subagent child threads.

use super::*;
use crate::types::EvaluatedPermission;

/// Project one canonical internal Thread identity onto the Managed public wire.
///
/// The neutral Session root is keyed by `session_id`; coordinated children
/// already own stable `sthr_` identities. Managed clients must never observe the
/// root's internal identity or the retired `<session_id>:primary` sentinel, so
/// this is the sole public/internal Thread ID codec for the adapter.
pub(crate) fn public_thread_id(session_id: &str, internal_thread_id: &str) -> String {
    if internal_thread_id == session_id {
        format!(
            "sthr_{}",
            awaken_session_contract::stable_fingerprint(
                &("managed-primary-thread-v1", session_id,)
            )
        )
    } else {
        internal_thread_id.to_string()
    }
}

/// Resolve one Managed public Thread identity back to the canonical internal
/// Thread key. Membership is still validated against the Session aggregate by
/// the caller; this function owns only the reversible root/child mapping.
pub(crate) fn internal_thread_id(session_id: &str, public_thread_id: &str) -> String {
    if public_thread_id == self::public_thread_id(session_id, session_id) {
        session_id.to_string()
    } else {
        public_thread_id.to_string()
    }
}

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
        primary_references_event: bool,
    ) -> Option<Event> {
        if thread_id == session_id {
            let Some(owner_thread_id) = owner_thread_id else {
                return Some(event);
            };
            if owner_thread_id == session_id {
                return Some(event);
            }
            match &mut event.kind {
                // `session.error` is the public failure projection for every
                // Session-owned Run, including coordinated children. Hiding a
                // child-owned error while cross-posting its terminal Thread
                // status would leave the primary Session history causally
                // incomplete.
                OutboundKind::SessionError { .. } => return Some(event),
                OutboundKind::AgentToolUse {
                    session_thread_id, ..
                } if primary_references_event => {
                    *session_thread_id = Some(owner_thread_id.to_string());
                    return Some(event);
                }
                OutboundKind::AgentCustomToolUse {
                    session_thread_id, ..
                }
                | OutboundKind::AgentToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Ask),
                    session_thread_id,
                    ..
                }
                | OutboundKind::AgentMcpToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Ask),
                    session_thread_id,
                    ..
                } => {
                    *session_thread_id = Some(owner_thread_id.to_string());
                    return Some(event);
                }
                _ => return None,
            }
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
        if matches!(
            &event.kind,
            OutboundKind::UserToolConfirmation {
                session_thread_id: Some(target),
                ..
            }
                | OutboundKind::UserCustomToolResult {
                    session_thread_id: Some(target),
                    ..
                }
                | OutboundKind::UserToolResult {
                    session_thread_id: Some(target),
                    ..
                }
                if target == thread_id
        ) {
            return Some(event);
        }
        event.kind = match event.kind {
            OutboundKind::AgentThreadMessageSent {
                to_session_thread_id,
                content,
                ..
            } if to_session_thread_id == thread_id => OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id: public_thread_id(session_id, session_id),
                from_agent_name: None,
                content,
            },
            OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id,
                content,
                ..
            } if from_session_thread_id == thread_id => OutboundKind::AgentThreadMessageSent {
                to_session_thread_id: public_thread_id(session_id, session_id),
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

    /// The Session's primary Thread projection (`BetaManagedAgentsSessionThread`).
    /// A Session has one public `sthr_` primary Thread; coordinated Agent and
    /// advisor work extends this list under stable child Thread identities
    /// derived by the Runtime authority.
    fn primary_thread(record: &SessionRecord) -> SessionThread {
        let session = &record.session;
        SessionThread {
            id: public_thread_id(&session.id, &session.id),
            kind: "session_thread",
            session_id: session.id.clone(),
            parent_thread_id: None,
            agent: SessionThreadAgent::from(&session.agent).into(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            archived_at: session.archived_at.clone(),
            status: Self::thread_status(session.status),
            stats: None,
            usage: record.primary_thread_usage.clone(),
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
        agent: impl Into<crate::types::SessionThreadAgentValue>,
    ) -> SessionThread {
        SessionThread {
            id: thread_id.to_string(),
            kind: "session_thread",
            session_id: session.id.clone(),
            parent_thread_id: Some(public_thread_id(&session.id, &session.id)),
            agent: agent.into(),
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
        let internal_thread_id = internal_thread_id(id, thread_id);
        let primary = Self::primary_thread(record);
        if internal_thread_id == id {
            return Ok(primary);
        }
        record
            .child_threads
            .iter()
            .find(|thread| thread.id == internal_thread_id)
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
        let internal_thread_id = internal_thread_id(id, thread_id);
        if internal_thread_id != id
            && !record
                .child_threads
                .iter()
                .any(|thread| thread.id == internal_thread_id)
        {
            return Err(StateError::NotFound);
        }
        let primary_answerable_event_ids = record.primary_answerable_event_ids();
        let events = record
            .events
            .iter()
            .cloned()
            .filter_map(|event| {
                let owner = record.event_thread_owners.get(&event.id);
                let primary_references_event =
                    primary_answerable_event_ids.contains(event.id.as_str());
                Self::project_event_for_thread(
                    id,
                    &internal_thread_id,
                    event,
                    owner.map(String::as_str),
                    primary_references_event,
                )
            })
            .collect::<Vec<_>>();
        let page = paginate_by_id(&events, cursor, limit, |event| event.id.as_str())
            .map_err(|_| RunError::bad_request("unknown pagination cursor"))?;
        Ok(ListEventsResponse {
            data: page.items.to_vec(),
            next_page: page.next_page,
        })
    }

    /// Apply the same record-aware projection to one committed live Event that
    /// paginated listing uses. The record may already be gone for the terminal
    /// `session.deleted` broadcast; that ownerless Session event remains
    /// visible, while no child-only event can be fabricated after deletion.
    pub(crate) fn project_committed_event_for_thread(
        &self,
        session_id: &str,
        thread_id: &str,
        event: Event,
    ) -> Option<Event> {
        let sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get(session_id) else {
            return Self::project_event_for_thread(session_id, thread_id, event, None, false);
        };
        let owner = record.event_thread_owners.get(&event.id);
        let primary_references_event = record
            .primary_answerable_event_ids()
            .contains(event.id.as_str());
        Self::project_event_for_thread(
            session_id,
            thread_id,
            event,
            owner.map(String::as_str),
            primary_references_event,
        )
    }

    /// `POST /v1/sessions/{id}/threads/{thread_id}/archive`. Archiving the primary
    /// thread archives the session; archiving a subagent child commits the one
    /// durable Thread disposition through the parent Session partition, then
    /// lets the ordinary warm/cold projector emit the terminal wire event.
    pub async fn archive_thread(
        &self,
        id: &str,
        thread_id: &str,
    ) -> Result<SessionThread, StateError> {
        self.ensure_session(id).await?;
        let internal_thread_id = internal_thread_id(id, thread_id);
        if internal_thread_id == id {
            self.archive_session(id).await?;
            return self.get_thread(id, thread_id);
        }
        let status = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            let child = record
                .child_threads
                .iter()
                .find(|thread| thread.id == internal_thread_id)
                .ok_or(StateError::NotFound)?;
            child.status
        };
        if status == SessionThreadStatus::Terminated {
            return self.get_thread(id, &internal_thread_id);
        }
        if status != SessionThreadStatus::Idle {
            return Err(StateError::Conflict);
        }
        self.application
            .archive_session_thread(id, &internal_thread_id)
            .await
            .map_err(StateError::Run)?;
        // Do not mutate the disposable Thread first. The disposition read in
        // this sole projector is what makes success, retry and restart converge.
        self.refresh_committed_events(id).await?;
        let archived = self.get_thread(id, &internal_thread_id)?;
        if archived.status != SessionThreadStatus::Terminated {
            return Err(StateError::Run(RunError::internal(
                "coordinated Thread archive committed without an Archived disposition",
            )));
        }
        Ok(archived)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_internal_thread_id_codec_follows_the_decision_table() {
        // Cause/effect graph: C1=the internal Thread key is the Session root;
        // C2=it is a coordinated child; C3=the input is the derived public root;
        // C4=it is a public child; C5=it is the retired primary sentinel.
        // E1=project one stable `sthr_` root id without
        // leaking the Session id or retired sentinel; E2=preserve the child id;
        // E3=resolve the public root to the Session key; E4=preserve the child
        // key; E5=preserve the retired sentinel as an unknown key so every
        // membership-checking route fails closed. Constraints: C1 xor C2 and
        // exactly one of C3-C5. Decision table:
        // | Rule | Direction | Root | Child | Retired | Effect |
        // | R1 | internal -> public | T | F | F | E1 |
        // | R2 | internal -> public | F | T | F | E2 |
        // | R3 | public -> internal | T | F | F | E3 |
        // | R4 | public -> internal | F | T | F | E4 |
        // | R5 | public -> internal | F | F | T | E5 |
        let session_id = "sesn_codec";
        let child_id = "sthr_child";

        let primary = public_thread_id(session_id, session_id);
        assert!(primary.starts_with("sthr_"), "R1/E1: {primary}");
        assert_ne!(primary, session_id, "R1/E1");
        assert!(!primary.contains(":primary"), "R1/E1");
        assert_eq!(
            public_thread_id(session_id, session_id),
            primary,
            "R1/E1 stable"
        );
        assert_eq!(public_thread_id(session_id, child_id), child_id, "R2/E2");
        assert_eq!(
            internal_thread_id(session_id, &primary),
            session_id,
            "R3/E3"
        );
        assert_eq!(internal_thread_id(session_id, child_id), child_id, "R4/E4");
        let retired_primary = format!("{session_id}:primary");
        assert_eq!(
            internal_thread_id(session_id, &retired_primary),
            retired_primary,
            "R5/E5"
        );
    }

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
