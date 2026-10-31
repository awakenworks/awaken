//! Session thread projections for [`ManagedState`]: the primary thread and
//! subagent child threads.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
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
            awaken_agent_contract::stable_fingerprint(&("managed-primary-thread-v1", session_id,))
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
        event: Event,
        owner_thread_id: Option<&str>,
        primary_references_event: bool,
    ) -> Option<Event> {
        Self::project_event_for_thread_cow(
            session_id,
            thread_id,
            std::borrow::Cow::Owned(event),
            owner_thread_id,
            primary_references_event,
        )
        .map(std::borrow::Cow::into_owned)
    }

    /// One projection kernel for owned live events and borrowed history. Clone
    /// only a visible event whose wire perspective actually changes.
    fn project_event_for_thread_cow<'a>(
        session_id: &str,
        thread_id: &str,
        mut event: std::borrow::Cow<'a, Event>,
        owner_thread_id: Option<&str>,
        primary_references_event: bool,
    ) -> Option<std::borrow::Cow<'a, Event>> {
        if thread_id == session_id {
            let Some(owner_thread_id) = owner_thread_id else {
                return Some(event);
            };
            if owner_thread_id == session_id {
                return Some(event);
            }
            match &event.kind {
                // `session.error` is the public failure projection for every
                // Session-owned Run, including coordinated children. Hiding a
                // child-owned error while cross-posting its terminal Thread
                // status would leave the primary Session history causally
                // incomplete.
                OutboundKind::SessionError { .. } => return Some(event),
                OutboundKind::AgentToolUse { .. } if primary_references_event => {}
                OutboundKind::AgentCustomToolUse { .. }
                | OutboundKind::AgentToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Ask),
                    ..
                }
                | OutboundKind::AgentMcpToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Ask),
                    ..
                } => {}
                _ => return None,
            }
            match &mut event.to_mut().kind {
                OutboundKind::AgentCustomToolUse {
                    session_thread_id, ..
                }
                | OutboundKind::AgentToolUse {
                    session_thread_id, ..
                }
                | OutboundKind::AgentMcpToolUse {
                    session_thread_id, ..
                } => {
                    *session_thread_id = Some(owner_thread_id.to_string());
                }
                _ => unreachable!("only cross-posted tools need a perspective change"),
            }
            return Some(event);
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
        let kind = match &event.kind {
            OutboundKind::AgentThreadMessageSent {
                to_session_thread_id,
                content,
                ..
            } if to_session_thread_id == thread_id => OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id: public_thread_id(session_id, session_id),
                from_agent_name: None,
                content: content.clone(),
            },
            OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id,
                content,
                ..
            } if from_session_thread_id == thread_id => OutboundKind::AgentThreadMessageSent {
                to_session_thread_id: public_thread_id(session_id, session_id),
                to_agent_name: None,
                content: content.clone(),
            },
            _ => return None,
        };
        Some(std::borrow::Cow::Owned(Event {
            id: event.id.clone(),
            processed_at: event.processed_at.clone(),
            kind,
        }))
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

    fn primary_thread_time(record: &SessionRecord) -> ThreadTimeEvidence {
        ThreadTimeEvidence {
            created_at_unix_ms: record.root_time.created_at_unix_ms,
            updated_at_unix_ms: record.root_time.updated_at_unix_ms,
            archived_at_unix_ms: record.root_time.archived_at_unix_ms,
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
        self.list_threads_with_time(id)
            .map(|rows| rows.into_iter().map(|(thread, _)| thread).collect())
    }

    pub(crate) fn list_threads_with_time(
        &self,
        id: &str,
    ) -> Result<Vec<(SessionThread, ThreadTimeEvidence)>, StateError> {
        let sessions = self.sessions.lock()?;
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let mut threads = vec![(
            Self::primary_thread(record),
            Self::primary_thread_time(record),
        )];
        threads.extend(record.child_threads.iter().cloned().map(|thread| {
            let time = record
                .child_times
                .get(&thread.id)
                .copied()
                .unwrap_or_default();
            (thread, time)
        }));
        Ok(threads)
    }

    /// `GET /v1/sessions/{id}/threads/{thread_id}` — the primary or a child thread.
    pub fn get_thread(&self, id: &str, thread_id: &str) -> Result<SessionThread, StateError> {
        self.get_thread_with_time(id, thread_id)
            .map(|(thread, _)| thread)
    }

    pub(crate) fn get_thread_with_time(
        &self,
        id: &str,
        thread_id: &str,
    ) -> Result<(SessionThread, ThreadTimeEvidence), StateError> {
        let sessions = self.sessions.lock()?;
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        let internal_thread_id = internal_thread_id(id, thread_id);
        let primary = Self::primary_thread(record);
        if internal_thread_id == id {
            return Ok((primary, Self::primary_thread_time(record)));
        }
        record
            .child_threads
            .iter()
            .find(|thread| thread.id == internal_thread_id)
            .map(|thread| {
                (
                    thread.clone(),
                    record
                        .child_times
                        .get(&thread.id)
                        .copied()
                        .unwrap_or_default(),
                )
            })
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
        self.list_thread_events_with_time(id, thread_id, cursor, limit, false)
    }

    pub(crate) fn list_thread_events_with_time(
        &self,
        id: &str,
        thread_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        time_v2: bool,
    ) -> Result<ListEventsResponse, StateError> {
        self.list_thread_events_filtered_with_time(
            id,
            thread_id,
            cursor,
            limit,
            time_v2,
            false,
            |_| true,
            |_| true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list_thread_events_filtered_with_time(
        &self,
        id: &str,
        thread_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        time_v2: bool,
        requires_time: bool,
        include: impl Fn(&Event) -> bool,
        could_match_without_time: impl Fn(&Event) -> bool,
    ) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock()?;
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
        Self::project_event_page_with_time(
            record,
            id,
            &internal_thread_id,
            cursor,
            limit,
            false,
            time_v2,
            requires_time,
            include,
            could_match_without_time,
        )
    }

    /// Session and Thread endpoints share filtering, ordering, cursor errors,
    /// and materialization. The published index resolves only candidate cursor
    /// positions; visibility and caller filtering still precede cursor admission.
    /// Only one page plus a matching lookahead is projected from its suffix.
    /// `include` must be a stable per-event predicate, as the wire filters are.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(super) fn project_event_page(
        record: &SessionRecord,
        session_id: &str,
        thread_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        descending: bool,
        include: impl Fn(&Event) -> bool,
    ) -> Result<ListEventsResponse, StateError> {
        Self::project_event_page_with_time(
            record,
            session_id,
            thread_id,
            cursor,
            limit,
            descending,
            false,
            false,
            include,
            |_| true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn project_event_page_with_time(
        record: &SessionRecord,
        session_id: &str,
        thread_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        descending: bool,
        time_v2: bool,
        requires_time: bool,
        include: impl Fn(&Event) -> bool,
        could_match_without_time: impl Fn(&Event) -> bool,
    ) -> Result<ListEventsResponse, StateError> {
        let (cursor, limit) =
            awaken_agent_contract::page::normalize_history_page_request(cursor, limit);
        let answerable = record.primary_answerable_event_ids();
        let project_at = |position| {
            record.events.get(position).and_then(|event| {
                Self::project_event_for_thread_cow(
                    session_id,
                    thread_id,
                    std::borrow::Cow::Borrowed(event),
                    record
                        .event_thread_owners
                        .get(&event.id)
                        .map(String::as_str),
                    answerable.contains(event.id.as_str()),
                )
                .map(|event| {
                    if time_v2 {
                        let mut event = event.into_owned();
                        event.processed_at = record.event_times.get(&event.id).map(|time| {
                            awaken_session_application_contract::epoch_millis_to_rfc3339(*time)
                        });
                        std::borrow::Cow::Owned(event)
                    } else {
                        event
                    }
                })
            })
        };
        if time_v2
            && requires_time
            && (0..record.events.len()).any(|position| {
                project_at(position).is_some_and(|event| {
                    could_match_without_time(&event) && event.processed_at.is_none()
                })
            })
        {
            return Err(StateError::Run(RunError::unavailable(
                "historical event time is unknown; time-filtered results cannot be completed",
            )));
        }
        let cursor_position = cursor
            .map(|cursor| {
                let positions = record
                    .event_positions(cursor)
                    .ok_or_else(|| RunError::bad_request("unknown pagination cursor"))?;
                (0..positions.len())
                    .map(|offset| {
                        positions[if descending {
                            positions.len() - 1 - offset
                        } else {
                            offset
                        }]
                    })
                    .find(|&position| {
                        project_at(position)
                            .is_some_and(|event| event.id == cursor && include(&event))
                    })
                    .ok_or_else(|| RunError::bad_request("unknown pagination cursor"))
            })
            .transpose()?;
        let boundary = if descending {
            cursor_position.unwrap_or(record.events.len())
        } else {
            cursor_position.map_or(0, |position| position + 1)
        };
        let scan = if descending {
            0..boundary
        } else {
            boundary..record.events.len()
        };
        let events = scan
            .map(|position| {
                if descending {
                    boundary - 1 - position
                } else {
                    position
                }
            })
            .filter_map(project_at)
            .filter(|event| include(event))
            .take(limit + 1)
            .collect::<Vec<_>>();
        // The canonical paginator still owns continuation construction and the
        // bounded page size. Cursor resolution already selected this suffix.
        let page = paginate_by_id(&events, None, Some(limit), |event| event.id.as_str())
            .map_err(|_| RunError::bad_request("unknown pagination cursor"))?;
        Ok(ListEventsResponse {
            data: page
                .items
                .iter()
                .map(|event| event.as_ref().clone())
                .collect(),
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
        // A poisoned cache cannot prove the event's thread owner: the live
        // stream drops it rather than fabricate visibility.
        let sessions = self.sessions.lock().ok()?;
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
            let sessions = self.sessions.lock()?;
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
        self.roles
            .termination
            .archive_session_thread(
                &awaken_session_types::SessionId::from(id),
                &internal_thread_id,
            )
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
    fn history_projection_borrows_unchanged_payloads_and_hides_before_copying() {
        // C1 primary/child view; C2 no owner/root/own child/other child;
        // C3 ordinary large payload (no perspective rewrite). R1 visible =>
        // Borrowed exact allocation; R2 invisible => None. This guards the
        // allocation boundary, independently of equal JSON response tests.
        let event = Event {
            id: "evt-large".into(),
            processed_at: None,
            kind: OutboundKind::AgentMessage {
                content: vec![ContentBlock::text("x".repeat(1024 * 1024))],
            },
        };
        for (view, owner, visible) in [
            ("root", None, true),
            ("root", Some("root"), true),
            ("root", Some("child"), false),
            ("child", Some("child"), true),
            ("child", Some("other"), false),
            ("child", None, false),
        ] {
            let projected = ManagedState::project_event_for_thread_cow(
                "root",
                view,
                std::borrow::Cow::Borrowed(&event),
                owner,
                false,
            );
            assert_eq!(projected.is_some(), visible, "{view}/{owner:?}");
            if let Some(projected) = projected {
                assert!(matches!(projected, std::borrow::Cow::Borrowed(_)));
                assert!(std::ptr::eq(projected.as_ref(), &event));
            }
        }
    }

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
