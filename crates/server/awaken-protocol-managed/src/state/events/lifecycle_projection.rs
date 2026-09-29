// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
//! Child lifecycle, disposition, delegation, and aggregate terminal projection.

use super::*;
use awaken_managed_session_events::{CoordinatedThread, ThreadStatus};

impl ManagedState {
    pub(super) fn public_run_stop_reason(
        record: &SessionRecord,
        owner_thread_id: Option<&str>,
        state: &awaken_agent_contract::agent::run::RunState,
        pending: Option<&Pending>,
        await_reason: Option<&awaken_agent_contract::agent::awaiting::AwaitReason>,
    ) -> Result<StopReason, StateError> {
        timeline_encoding::resolve_stop(
            record,
            owner_thread_id,
            &awaken_managed_session_events::terminal_stop(state, pending, await_reason, false),
        )
    }

    /// The coordinated children as the timeline domain sees them.
    pub(super) fn coordinated_threads(record: &SessionRecord) -> Vec<CoordinatedThread> {
        record
            .child_threads
            .iter()
            .map(|child| CoordinatedThread {
                thread: child.id.clone(),
                advisor: child.agent.is_advisor(),
                status: match child.status {
                    SessionThreadStatus::Running => ThreadStatus::Running,
                    SessionThreadStatus::Rescheduling => ThreadStatus::Rescheduling,
                    SessionThreadStatus::Idle => ThreadStatus::Idle,
                    SessionThreadStatus::Terminated => ThreadStatus::Terminated,
                },
            })
            .collect()
    }

    pub(super) fn append_child_lifecycle_projection(
        &self,
        record: &mut SessionRecord,
        events: &[RunLifecycleEvent],
        historical_pending: &std::collections::HashMap<RunLifecycleCursor, Pending>,
        snapshots: &std::collections::HashMap<
            String,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
    ) -> Result<(), StateError> {
        let mut threads = Self::coordinated_threads(record);
        let projection = awaken_managed_session_events::project_child_lifecycle(
            &mut threads,
            events,
            historical_pending,
            snapshots,
        )
        .map_err(projection_error)?;
        for fact in &projection.facts {
            record.publish_fact(fact, None)?;
        }
        record
            .checkpoint
            .progress
            .terminal_cursors
            .extend(projection.terminal_cursors);
        Ok(())
    }

    pub(super) fn append_child_disposition_projection(
        &self,
        record: &mut SessionRecord,
        dispositions: &std::collections::HashMap<String, awaken_agent_contract::ThreadDisposition>,
    ) -> Result<(), StateError> {
        let mut threads = Self::coordinated_threads(record);
        for fact in awaken_managed_session_events::archive_children(&mut threads, dispositions) {
            record.publish_fact(&fact, None)?;
        }
        Ok(())
    }

    /// Project the absorbing parent Session terminal over every derived Thread.
    /// `PersistedSession.execution` remains the only terminal authority: child
    /// status and events here are disposable wire projections of that fact, not
    /// independently persisted child dispositions. Child terminals precede the
    /// aggregate terminal so each child SSE can close while the primary stream
    /// continues until `session.status_terminated`.
    pub(super) fn append_parent_terminal_projection(
        &self,
        record: &mut SessionRecord,
    ) -> Result<(), StateError> {
        if record.session.status != SessionStatus::Terminated {
            return Ok(());
        }
        let session_id = record.session.id.clone();
        let primary = awaken_managed_session_events::FactKey::new(
            session_id.as_str(),
            awaken_managed_session_events::FactRole::ParentTerminalPrimaryStatusTerminated,
            awaken_managed_session_events::FactProvenance::ParentTerminal,
        );
        let primary_published = record.has_event_id(&record.fact_id(&primary));
        // Child and primary terminals are Thread statuses, so any aggregate
        // terminal already published, whatever its origin, closes the Session.
        let session_terminated_published = record
            .events
            .iter()
            .any(|event| matches!(event.kind, OutboundKind::SessionStatusTerminated { .. }));
        let mut threads = Self::coordinated_threads(record);
        for fact in awaken_managed_session_events::terminate_session(
            &session_id,
            &mut threads,
            primary_published,
            session_terminated_published,
        ) {
            record.publish_fact(&fact, None)?;
        }
        Ok(())
    }

    /// Whether the latest public lifecycle edge for the primary Thread is open.
    /// The committed event tail is the existing projector idempotency authority;
    /// no parallel primary-status cell is maintained.
    pub(super) fn primary_thread_status_is_open(record: &SessionRecord) -> bool {
        let primary_id = public_thread_id(&record.session.id, &record.session.id);
        record
            .events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                OutboundKind::SessionThreadStatusRunning {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(true),
                OutboundKind::SessionThreadStatusRescheduled {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(true),
                OutboundKind::SessionThreadStatusIdle {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(false),
                OutboundKind::SessionThreadStatusTerminated {
                    session_thread_id, ..
                } if session_thread_id == &primary_id => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Sole Runtime relationship -> Managed child Thread/event projector. Warm
    /// refresh and cold rehydration call this same function. Its inputs are real
    /// child Thread links, parent-partition transcripts, and ordinary Run
    /// lifecycle facts; a synchronous legacy `DelegatedRun.run_id` is never
    /// reinterpreted as a Thread id.
    pub(super) fn append_delegation_projections(
        &self,
        record: &mut SessionRecord,
        evidence: DelegationProjectionEvidence<'_>,
    ) -> Result<(), StateError> {
        let DelegationProjectionEvidence {
            links,
            snapshots,
            transcripts,
            lifecycle_events,
            latest_run_states,
            pending,
            historical_pending,
            dispositions,
            usage,
        } = evidence;
        let mut seen = std::collections::HashSet::new();
        let resolved = links
            .iter()
            .map(|link| {
                if !seen.insert(link.thread_id.0.clone()) {
                    return Err(StateError::Run(RunError::internal(
                        "Runtime returned a duplicate coordinated Thread link",
                    )));
                }
                Self::coordinated_thread_agent(record, link).map(|agent| (link, agent))
            })
            .collect::<Result<Vec<_>, _>>()?;
        record
            .child_threads
            .retain(|child| seen.contains(&child.id));
        for (link, agent) in resolved {
            let thread_id = link.thread_id.0.clone();
            let existing = record
                .child_threads
                .iter()
                .position(|thread| thread.id == thread_id);
            let is_new = existing.is_none();
            let index = existing.unwrap_or_else(|| {
                let child = Self::child_thread(&record.session, &thread_id, agent);
                record.child_threads.push(child);
                record.child_threads.len() - 1
            });
            record.child_threads[index].usage = usage.get(&thread_id).cloned().flatten();
            if is_new {
                for fact in awaken_managed_session_events::link_facts(link) {
                    record.publish_fact(&fact, None)?;
                }
            }
            debug_assert_eq!(record.child_threads[index].id, thread_id);
        }
        self.append_coordination_cross_posts(record, links)?;
        for link in links {
            let thread_id = link.thread_id.0.as_str();
            let child = record
                .child_threads
                .iter()
                .find(|child| child.id == thread_id)
                .expect("validated coordinated Thread is cached");
            let advisor_model = match &child.agent {
                crate::types::SessionThreadAgentValue::Advisor(advisor) => {
                    Some(advisor.model.clone())
                }
                crate::types::SessionThreadAgentValue::Agent(_) => None,
            };
            let child_lifecycle_events = lifecycle_events
                .iter()
                .filter(|event| event.thread_id.0 == thread_id)
                .cloned()
                .collect::<Vec<_>>();
            if transcripts.contains_key(thread_id)
                && let Some(snapshot) = snapshots.get(thread_id)
            {
                Self::append_run_observation_projections(record, thread_id, snapshot)?;
            }
            self.append_child_transcript_projection(
                record,
                ChildTranscriptProjection {
                    thread_id,
                    advisor_model: advisor_model.as_deref(),
                    messages: transcripts
                        .get(thread_id)
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                    lifecycle_events: &child_lifecycle_events,
                    latest_run_id: link.latest_run_id.as_ref(),
                    latest_run_state: latest_run_states.get(thread_id),
                    pending: pending.get(thread_id),
                    pending_source_run_id: lifecycle_events
                        .iter()
                        .rev()
                        .find(|event| {
                            event.thread_id.0 == thread_id
                                && event.kind == RunLifecycleEventKind::Awaiting
                        })
                        .map(|event| &event.run_id),
                    historical_pending,
                },
            )?;
        }
        self.append_child_lifecycle_projection(
            record,
            lifecycle_events,
            historical_pending,
            snapshots,
        )?;
        // Archive is an absorbing Thread disposition, so it is projected after
        // historical Run lifecycle. On warm refresh it prevents later lifecycle
        // pages from reviving the child; on cold rebuild it closes the same
        // created/running/message/idle history with one terminal event.
        self.append_child_disposition_projection(record, dispositions)?;
        Ok(())
    }
}
