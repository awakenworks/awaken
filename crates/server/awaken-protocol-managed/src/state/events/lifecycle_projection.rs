// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
//! Child lifecycle, disposition, delegation, and aggregate terminal projection.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_managed_session_events::{CoordinatedThread, OrderIndex, ThreadStatus};

// Coordination opening/message facts precede transcript qualification; every
// other child lifecycle fact follows it. Both filters borrow this one rule.
fn precedes_child_transcript(fact: &awaken_managed_session_events::DerivedSessionEvent) -> bool {
    matches!(
        fact.key.role,
        awaken_managed_session_events::ManagedEventType::CoordinationStatusRunning
            | awaken_managed_session_events::ManagedEventType::CoordinationMessageSent
    )
}

impl ManagedState {
    /// Select child Running, cross-post, and lifecycle facts in one committed
    /// source-order fold. A prior warm child status is never consulted.
    pub(super) fn plan_child_activity_projection(
        &self,
        links: &[CoordinatedThreadLink],
        events: &[RunLifecycleEvent],
        historical_pending: &std::collections::HashMap<RunLifecycleCursor, Pending>,
        snapshots: &std::collections::HashMap<
            String,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
        plan: &awaken_managed_session_events::CoordinationEvents,
        index: &OrderIndex<'_>,
    ) -> Result<
        (
            awaken_managed_session_events::ChildLifecycle,
            Vec<CoordinatedThread>,
        ),
        StateError,
    > {
        let mut threads = awaken_managed_session_events::initial_coordinated_threads(links);
        let projection = awaken_managed_session_events::project_child_activity(
            &mut threads,
            &plan.cross_posts,
            &plan.coordination_orders,
            events,
            historical_pending,
            snapshots,
            index,
        )
        .map_err(projection_error)?;
        Ok((projection, threads))
    }

    pub(super) fn append_child_disposition_projection(
        &self,
        record: &mut SessionRecord,
        threads: &mut [CoordinatedThread],
        dispositions: &std::collections::HashMap<String, awaken_agent_contract::ThreadDisposition>,
    ) -> Result<(), StateError> {
        for fact in awaken_managed_session_events::archive_children(threads, dispositions) {
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
        threads: &mut [CoordinatedThread],
    ) -> Result<(), StateError> {
        if record.session.status != SessionStatus::Terminated {
            return Ok(());
        }
        let session_id = record.session.id.clone();
        let primary = awaken_managed_session_events::EventIdentity::new(
            session_id.as_str(),
            awaken_managed_session_events::ManagedEventType::ParentTerminalPrimaryStatusTerminated,
            awaken_managed_session_events::FactProvenance::ParentTerminal,
        );
        let primary_published = record.has_event_id(&record.fact_id(&primary));
        // Child and primary terminals are Thread statuses, so any aggregate
        // terminal already published, whatever its origin, closes the Session.
        let session_terminated_published = record
            .events
            .iter()
            .any(|event| matches!(event.kind, OutboundKind::SessionStatusTerminated { .. }));
        for fact in awaken_managed_session_events::terminate_session(
            &session_id,
            threads,
            primary_published,
            session_terminated_published,
        ) {
            record.publish_fact(&fact, None)?;
        }
        Ok(())
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
        plan: &awaken_managed_session_events::CoordinationEvents,
    ) -> Result<Vec<CoordinatedThread>, StateError> {
        let DelegationProjectionEvidence {
            order_index,
            links,
            snapshots,
            transcripts,
            lifecycle_events,
            latest_run_states,
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
            let index = existing.unwrap_or_else(|| {
                let child = Self::child_thread(&record.session, &thread_id, agent);
                record.child_threads.push(child);
                record.child_threads.len() - 1
            });
            record.child_threads[index].usage = usage.get(&thread_id).cloned().flatten();
            if existing.is_none() {
                for fact in awaken_managed_session_events::link_facts(link) {
                    record.publish_fact(&fact, None)?;
                }
            }
            debug_assert_eq!(record.child_threads[index].id, thread_id);
        }
        let (activity, mut final_threads) = self.plan_child_activity_projection(
            links,
            lifecycle_events,
            historical_pending,
            snapshots,
            plan,
            order_index,
        )?;
        // Child transcript projection resolves answerable tool ids before
        // lifecycle Idle/usage consumes them. Keep that existing dependency
        // while the sole activity plan has already selected every Running key.
        for fact in activity
            .facts
            .iter()
            .filter(|fact| precedes_child_transcript(fact))
        {
            record.publish_fact(fact, None)?;
        }
        for link in links {
            let thread_id = link.thread_id.0.as_str();
            let child = record
                .child_threads
                .iter()
                .find(|child| child.id == thread_id)
                .expect("validated coordinated Thread is cached");
            let advisor_model = child
                .agent
                .as_advisor()
                .map(|advisor| advisor.model.clone());
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
                },
                order_index,
            )?;
        }
        for fact in activity
            .facts
            .iter()
            .filter(|fact| !precedes_child_transcript(fact))
        {
            record.publish_fact(fact, None)?;
        }
        record
            .checkpoint
            .progress
            .terminal_cursors
            .extend(activity.terminal_cursors);
        for projected in &final_threads {
            if let Some(child) = record
                .child_threads
                .iter_mut()
                .find(|child| child.id == projected.thread)
            {
                child.status = match projected.status {
                    ThreadStatus::Running => SessionThreadStatus::Running,
                    ThreadStatus::Rescheduling => SessionThreadStatus::Rescheduling,
                    ThreadStatus::Idle => SessionThreadStatus::Idle,
                    ThreadStatus::Terminated => SessionThreadStatus::Terminated,
                };
            }
        }
        // Archive is an absorbing Thread disposition, so it is projected after
        // historical Run lifecycle. On warm refresh it prevents later lifecycle
        // pages from reviving the child; on cold rebuild it closes the same
        // created/running/message/idle history with one terminal event.
        self.append_child_disposition_projection(record, &mut final_threads, dispositions)?;
        Ok(final_threads)
    }
}
