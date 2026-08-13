//! Event driving for [`ManagedState`]: projecting committed turns/outcomes,
//! the live-inbox surface, and `send_events`/`list_events`.

use super::*;
use awaken_agent_contract::agent::delegation::DelegationStatus;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEventKind};

struct DelegateCall {
    run_id: String,
    agent_name: String,
    sent: Vec<ContentBlock>,
    received: Vec<ContentBlock>,
    status: DelegationStatus,
}

impl ManagedState {
    fn map_activity_error(error: awaken_session_application::SessionActivityError) -> StateError {
        match error {
            awaken_session_application::SessionActivityError::NotFound => StateError::NotFound,
            awaken_session_application::SessionActivityError::Terminal => StateError::Archived,
            awaken_session_application::SessionActivityError::NotReady => StateError::Run(
                RunError::classified("session_not_ready", "Session realization has not completed"),
            ),
            awaken_session_application::SessionActivityError::BudgetReached => {
                StateError::Run(RunError::classified(
                    "budget_reached",
                    "Session list-cost budget has been reached",
                ))
            }
            awaken_session_application::SessionActivityError::Conflict => StateError::Conflict,
            awaken_session_application::SessionActivityError::EpochExhausted => {
                StateError::Run(RunError::internal("Session activity epoch is exhausted"))
            }
            awaken_session_application::SessionActivityError::Unavailable(message) => {
                StateError::Run(RunError::internal(message))
            }
        }
    }

    pub(super) fn next_event_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    /// The session's live SSE broadcast sender, created on first use. Capacity is
    /// generous so a fast turn's preview burst doesn't lag a slow subscriber into
    /// `Lagged` (which the stream tolerates by skipping). Never removed.
    fn live_sender(&self, session_id: &str) -> broadcast::Sender<StreamFrame> {
        let mut live = self.live.lock().unwrap();
        live.entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(1024).0)
            .clone()
    }

    /// Open a live SSE subscription for `session_id`: the current committed-event
    /// snapshot (backfill) plus a receiver for frames published after this call.
    /// Subscribing *before* cloning the snapshot means no committed event can slip
    /// through the gap — an event that lands mid-call is on the receiver, and the
    /// caller dedupes it against the snapshot by id.
    pub fn stream_subscribe(
        &self,
        session_id: &str,
    ) -> Result<(Vec<Event>, broadcast::Receiver<StreamFrame>), StateError> {
        let rx = self.live_sender(session_id).subscribe();
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok((record.events.clone(), rx))
    }

    /// Publish each committed `Event` appended to `session_id` since `from` on the
    /// live broadcast, so an open SSE connection receives it without a reconnect.
    /// Best-effort: no subscriber (or a lagging one) is not an error.
    pub(super) fn broadcast_committed_from(
        &self,
        session_id: &str,
        record: &SessionRecord,
        from: usize,
    ) {
        if from >= record.events.len() {
            return;
        }
        if let Some(tx) = self.live.lock().unwrap().get(session_id) {
            for event in &record.events[from..] {
                let _ = tx.send(StreamFrame::Committed(event.clone()));
            }
        }
    }

    /// Resolve the public optional Thread selector onto the runtime's canonical
    /// thread keys. The primary Thread is projected with a public suffix, while
    /// the runtime has always keyed it by the Session id; child Thread ids are
    /// already the child Run keys. Keeping that translation here prevents the
    /// event handler and Thread routes from growing competing identity rules.
    fn interrupt_targets(
        &self,
        session_id: &str,
        requested_thread_id: Option<&str>,
    ) -> Result<Vec<String>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        let primary_id = format!("{session_id}:primary");
        if let Some(thread_id) = requested_thread_id {
            if thread_id == primary_id {
                return Ok(vec![session_id.to_string()]);
            }
            let child = record
                .child_threads
                .iter()
                .find(|thread| thread.id == thread_id)
                .ok_or_else(|| {
                    StateError::Run(RunError::bad_request(
                        "session_thread_id does not name a thread in this session",
                    ))
                })?;
            if child.status == SessionThreadStatus::Terminated {
                return Err(StateError::Run(RunError::bad_request(
                    "an archived or terminated session thread cannot be interrupted",
                )));
            }
            return Ok(vec![child.id.clone()]);
        }

        Ok(std::iter::once(session_id.to_string())
            .chain(
                record
                    .child_threads
                    .iter()
                    .filter(|thread| thread.status != SessionThreadStatus::Terminated)
                    .map(|thread| thread.id.clone()),
            )
            .collect())
    }

    fn public_inbound_kind(event: &InboundEvent) -> OutboundKind {
        match event {
            InboundEvent::UserMessage {
                content,
                session_thread_id,
                model,
            } => OutboundKind::UserMessage {
                content: content.clone(),
                session_thread_id: session_thread_id.clone(),
                model: model.clone(),
            },
            InboundEvent::SystemMessage { content } => OutboundKind::SystemMessage {
                content: content.clone(),
            },
            InboundEvent::UserToolConfirmation {
                tool_use_id,
                result,
                deny_message,
            } => OutboundKind::UserToolConfirmation {
                tool_use_id: tool_use_id.clone(),
                result: *result,
                deny_message: deny_message.clone(),
            },
            InboundEvent::UserCustomToolResult {
                custom_tool_use_id,
                content,
                is_error,
            } => OutboundKind::UserCustomToolResult {
                custom_tool_use_id: custom_tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
            InboundEvent::UserToolResult {
                tool_use_id,
                content,
                is_error,
            } => OutboundKind::UserToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
            InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => OutboundKind::UserDefineOutcome {
                description: description.clone(),
                rubric: rubric.clone(),
                max_iterations: *max_iterations,
            },
            InboundEvent::UserInterrupt { session_thread_id } => OutboundKind::UserInterrupt {
                session_thread_id: session_thread_id.clone(),
            },
        }
    }

    fn append_inbound_event(
        &self,
        session_id: &str,
        inbound: &InboundEvent,
    ) -> Result<String, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let start = record.events.len();
        let id = self.next_event_id();
        record.events.push(Event {
            id: id.clone(),
            kind: Self::public_inbound_kind(inbound),
            processed_at: None,
        });
        self.broadcast_committed_from(session_id, record, start);
        Ok(id)
    }

    fn mark_inbound_processed(&self, session_id: &str, event_id: &str) {
        if let Some(event) = self
            .sessions
            .lock()
            .unwrap()
            .get_mut(session_id)
            .and_then(|record| record.events.iter_mut().find(|event| event.id == event_id))
        {
            event.processed_at = Some(PROCESSED_AT.to_string());
        }
    }

    /// Start a create-time event batch without inventing a second executor. The
    /// create response is made `running` before this returns; the detached task
    /// then uses the exact `send_events` command used by the public events route.
    pub(crate) fn start_initial_events(
        self: &Arc<Self>,
        session_id: &str,
        events: Vec<InboundEvent>,
    ) -> Result<(), StateError> {
        if events.is_empty() {
            return Ok(());
        }
        {
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
            record.project_runtime_status(SessionStatus::Running);
        }
        let state = Arc::clone(self);
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            if let Err(error) = state
                .send_event_batch(
                    &session_id,
                    SendEventsRequest {
                        events,
                        user_profile_id: None,
                    },
                    true,
                )
                .await
            {
                tracing::warn!(%session_id, %error, "create-time initial events failed");
            }
        });
        Ok(())
    }

    fn delegate_calls(delegations: &[DelegatedRun], events: &[Event]) -> Vec<DelegateCall> {
        delegations
            .iter()
            .map(|delegation| {
                let sent = events.iter().find_map(|event| match &event.kind {
                    OutboundKind::AgentToolUse { input, .. }
                        if event.id == delegation.parent_call_id =>
                    {
                        Some(vec![ContentBlock::text(
                            input
                                .get("input")
                                .and_then(|value| value.as_str())
                                .unwrap_or_default(),
                        )])
                    }
                    _ => None,
                });
                let received = events.iter().find_map(|event| match &event.kind {
                    OutboundKind::AgentToolResult {
                        tool_use_id,
                        content,
                        ..
                    } if tool_use_id == &delegation.parent_call_id => Some(content.clone()),
                    _ => None,
                });
                DelegateCall {
                    run_id: delegation.run_id.0.clone(),
                    agent_name: delegation.agent_id.clone(),
                    sent: sent.unwrap_or_default(),
                    received: received.unwrap_or_default(),
                    status: delegation.status,
                }
            })
            .collect()
    }

    /// Sole Runtime relationship -> Managed child Thread/event projector. Live
    /// turns and restart recovery call the same function, so neither can invent a
    /// different identity, message direction, or lifecycle sequence.
    pub(super) fn append_delegation_projections(
        &self,
        record: &mut SessionRecord,
        delegations: &[DelegatedRun],
    ) {
        for d in Self::delegate_calls(delegations, &record.events) {
            let thread_id = d.run_id;
            let name = d.agent_name;
            let existing = record
                .child_threads
                .iter()
                .position(|thread| thread.id == thread_id);
            let is_new = existing.is_none();
            let index = existing.unwrap_or_else(|| {
                let child = Self::child_thread(&record.session, &thread_id, &name);
                record.child_threads.push(child);
                record.child_threads.len() - 1
            });
            let was_idle = record.child_threads[index].status == SessionThreadStatus::Idle;
            let completed = d.status == DelegationStatus::Completed;
            let mut kinds = Vec::new();
            if is_new {
                kinds.extend([
                    OutboundKind::SessionThreadCreated {
                        session_thread_id: thread_id.clone(),
                        agent_name: name.clone(),
                    },
                    OutboundKind::SessionThreadStatusRunning {
                        session_thread_id: thread_id.clone(),
                        agent_name: name.clone(),
                    },
                    OutboundKind::AgentThreadMessageSent {
                        to_session_thread_id: thread_id.clone(),
                        to_agent_name: Some(name.clone()),
                        content: d.sent,
                    },
                ]);
            }
            if completed && !was_idle {
                record.child_threads[index].status = SessionThreadStatus::Idle;
                record.child_threads[index].updated_at = PROCESSED_AT.to_string();
                kinds.extend([
                    OutboundKind::AgentThreadMessageReceived {
                        from_session_thread_id: thread_id.clone(),
                        from_agent_name: Some(name.clone()),
                        content: d.received,
                    },
                    OutboundKind::SessionThreadStatusIdle {
                        session_thread_id: thread_id,
                        agent_name: name,
                        stop_reason: StopReason::EndTurn,
                    },
                ]);
            }
            record.events.extend(kinds.into_iter().map(|kind| Event {
                id: self.next_event_id(),
                kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            }));
        }
    }

    fn lifecycle_cursor(&self, session_id: &str) -> Result<RunLifecycleCursor, StateError> {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|record| record.projected_lifecycle_cursor)
            .ok_or(StateError::NotFound)
    }

    async fn terminal_cursor_after(
        &self,
        session_id: &str,
        after: RunLifecycleCursor,
        outcome: &StepOutcome,
    ) -> Result<Option<RunLifecycleCursor>, StateError> {
        let Some(run_id) = outcome.run_id() else {
            return Ok(None);
        };
        const PAGE_SIZE: usize = 256;
        let mut cursor = after;
        let mut found = None;
        loop {
            let page = self
                .application
                .committed_run_lifecycle(session_id, cursor, PAGE_SIZE)
                .await
                .map_err(StateError::Run)?;
            let count = page.events.len();
            for event in page.events {
                if event.thread_id.0 == session_id
                    && &event.run_id == run_id
                    && &event.state == outcome.state()
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Awaiting
                            | RunLifecycleEventKind::Completed
                            | RunLifecycleEventKind::Failed
                            | RunLifecycleEventKind::Cancelled
                    )
                {
                    found = Some(event.cursor);
                }
            }
            if page.next_cursor == cursor || count < PAGE_SIZE {
                return Ok(found);
            }
            cursor = page.next_cursor;
        }
    }

    async fn append_committed_step(
        &self,
        session_id: &str,
        outcome: StepOutcome,
        preview_ids: PreviewAllocations,
        lifecycle_start: RunLifecycleCursor,
    ) -> Result<(), StateError> {
        let terminal_cursor = self
            .terminal_cursor_after(session_id, lifecycle_start, &outcome)
            .await?;
        if outcome.run_id().is_some() && terminal_cursor.is_none() {
            return Err(StateError::Run(RunError::internal(
                "committed Run terminal is missing from the lifecycle feed",
            )));
        }
        self.append_step(session_id, outcome, preview_ids, terminal_cursor)
    }

    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    /// Project a committed turn into events and append them. Preview allocations
    /// carry the ids minted for message and thinking starts; their corresponding
    /// buffered events reuse them so a client reconciles by id.
    pub(super) fn append_step(
        &self,
        session_id: &str,
        outcome: StepOutcome,
        mut preview_ids: PreviewAllocations,
        terminal_cursor: Option<RunLifecycleCursor>,
    ) -> Result<(), StateError> {
        let pending = outcome.pending();
        let delegated_runs = outcome.delegated_runs().to_vec();
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let (projected_tool_ids, prior_mcp_ids) = record.projected_tool_ids();
        let project_terminal =
            terminal_cursor.is_none_or(|cursor| record.projected_terminal_cursors.insert(cursor));
        // A concurrent GET may already have observed this process's committed
        // messages through the shared transcript and projected them. Message ids
        // deduplicate messages; the exact lifecycle cursor deduplicates its status
        // bracket regardless of whether the feed or local request wins the race.
        let new_messages = outcome
            .new_messages
            .iter()
            .filter(|message| record.projected_message_ids.insert(message.id.0.clone()))
            .cloned()
            .collect::<Vec<_>>();
        let projected = if project_terminal {
            project_step(
                &new_messages,
                outcome.state(),
                pending,
                &projected_tool_ids,
                prior_mcp_ids,
            )
        } else {
            project_messages_with_mcp_ids(
                &new_messages,
                pending,
                &projected_tool_ids,
                prior_mcp_ids,
            )
        };
        // Everything appended from here is republished on the live broadcast at the end.
        let start = record.events.len();
        // Each processing segment is bracketed `running` … `idle`; the running
        // marker leads before any fold or message.
        if project_terminal {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionStatusRunning {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // A transparent transient-retry recovery surfaces as `session.status_rescheduled`
        // between the running marker and the turn's output, so a client observes that
        // the runtime auto-recovered rather than seeing an unexplained pause.
        if project_terminal && outcome.rescheduled {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionStatusRescheduled {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // Compaction ran at BeforeInference, so its marker precedes the turn's
        // message events. `true` ⇒ this terminal step folded (emit-once upstream).
        if project_terminal && outcome.compacted {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::ThreadContextCompacted {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // A terminal run fault projects a `session.error` before the turn's idle,
        // so a streaming/listing client observes the failure. The neutral fault's
        // `code` classifies the SDK error variant + retry status; its `message` is
        // carried through.
        if project_terminal && let Some(failure) = outcome.failure() {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionError {
                    error: SessionError::classify(failure.code(), failure.message()),
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        for event in projected {
            // An `agent.message` (minted with no id) reuses the id its live preview
            // announced, so `event_start.event.id == agent.message.id` and the SDK
            // discards the accumulated preview on the buffered event. Other events, and
            // any message beyond the previewed count, mint a fresh id as before.
            let id = event.id.unwrap_or_else(|| match &event.kind {
                OutboundKind::AgentMessage { .. } => preview_ids
                    .next_message()
                    .unwrap_or_else(|| self.next_event_id()),
                OutboundKind::AgentThinking {} => preview_ids
                    .next_thinking()
                    .unwrap_or_else(|| self.next_event_id()),
                _ => self.next_event_id(),
            });
            record.events.push(Event {
                id,
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        self.append_delegation_projections(record, &delegated_runs);
        record.project_runtime_status(SessionStatus::Idle);
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// Append an outcome report: for each round, the agent's revision events then
    /// `span.outcome_evaluation_start` / `_end`, and a terminal `session.status_idle`.
    fn append_outcome(&self, session_id: &str, report: OutcomeReport) -> Result<(), StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        // Each round's durable evaluation record, collected as we project its events
        // and folded into the session object after the event-pushing borrow releases.
        let mut evaluations = Vec::new();
        let rounds = report
            .iterations
            .into_iter()
            .map(|mut round| {
                round
                    .messages
                    .retain(|message| record.projected_message_ids.insert(message.id.0.clone()));
                round
            })
            .collect::<Vec<_>>();
        let start = record.events.len();
        {
            let mut push = |id: Option<String>, kind: OutboundKind| {
                record.events.push(Event {
                    id: id.unwrap_or_else(|| {
                        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
                    }),
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            };
            push(None, OutboundKind::SessionStatusRunning {});
            for round in rounds {
                for event in project_messages(&round.messages, None) {
                    push(event.id, event.kind);
                }
                evaluations.push(project::outcome_evaluation(&round));
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationStart {
                        outcome_id: round.outcome_id.clone(),
                        iteration: round.iteration,
                    },
                );
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationOngoing {
                        outcome_id: round.outcome_id.clone(),
                        iteration: round.iteration,
                    },
                );
                push(
                    None,
                    OutboundKind::SpanOutcomeEvaluationEnd {
                        outcome_id: round.outcome_id,
                        iteration: round.iteration,
                        result: round.result,
                        explanation: round.explanation,
                    },
                );
            }
            push(
                None,
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::EndTurn,
                },
            );
        }
        // The session object carries the running list of evaluations that have graded
        // it, so a `GET /v1/sessions/{id}` reflects the outcomes that ran, not [].
        for evaluation in evaluations {
            if let Some(existing) = record
                .session
                .outcome_evaluations
                .iter_mut()
                .find(|existing| existing.outcome_id == evaluation.outcome_id)
            {
                *existing = evaluation;
            } else {
                record.session.outcome_evaluations.push(evaluation);
            }
        }
        record.project_runtime_status(SessionStatus::Idle);
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// Consume committed Run truth before projecting the Outcome boundary. This
    /// keeps tool calls and terminal lifecycle on the existing projection path;
    /// the Outcome report adds only evaluation facts and any still-unseen text.
    async fn project_outcome_drive(
        &self,
        session_id: &str,
        progress: OutcomeDrive,
    ) -> Result<(), StateError> {
        self.refresh_committed_events(session_id).await?;
        if let OutcomeDrive::Completed(report) = progress {
            self.append_outcome(session_id, report)?;
        }
        Ok(())
    }

    async fn continue_outcome_after_resume(&self, session_id: &str) -> Result<(), StateError> {
        if let Some(progress) = self.application.continue_outcome(session_id).await? {
            self.project_outcome_drive(session_id, progress).await?;
        }
        Ok(())
    }

    async fn process_inbound_event(
        &self,
        session_id: &str,
        agent_id: &str,
        user_profile_id: Option<String>,
        inbound: &InboundEvent,
    ) -> Result<(), StateError> {
        match inbound {
            InboundEvent::UserMessage { content, model, .. } => {
                let lifecycle_start = self.lifecycle_cursor(session_id)?;
                if let Some(model) = model {
                    self.application.rebind_model(session_id, model).await?;
                }
                let sink = Arc::new(PreviewSink::new(
                    self.live_sender(session_id),
                    self.event_seq.clone(),
                ));
                let outcome = self
                    .application
                    .run_session_message(
                        agent_id,
                        session_id,
                        content.clone(),
                        user_profile_id,
                        sink.clone(),
                    )
                    .await;
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.append_runtime_failure(session_id, &error)?;
                        return Err(StateError::Run(error));
                    }
                };
                self.refresh_cached_projection(&outcome.session)?;
                self.append_committed_step(
                    session_id,
                    outcome.step,
                    sink.take_allocations(),
                    lifecycle_start,
                )
                .await?;
            }
            InboundEvent::UserToolConfirmation {
                tool_use_id,
                result,
                deny_message,
            } => {
                let lifecycle_start = self.lifecycle_cursor(session_id)?;
                let decision = ToolPermissionDecision {
                    allow: matches!(result, ConfirmResult::Allow),
                    note: deny_message.clone(),
                };
                let outcome = self
                    .application
                    .resume(session_id, tool_use_id, decision)
                    .await?;
                self.append_committed_step(
                    session_id,
                    outcome,
                    PreviewAllocations::default(),
                    lifecycle_start,
                )
                .await?;
                self.continue_outcome_after_resume(session_id).await?;
            }
            InboundEvent::UserCustomToolResult {
                custom_tool_use_id,
                content,
                is_error,
            } => {
                let lifecycle_start = self.lifecycle_cursor(session_id)?;
                let outcome = self
                    .application
                    .resume_custom(
                        session_id,
                        custom_tool_use_id,
                        content.clone().unwrap_or_default(),
                        *is_error,
                    )
                    .await?;
                self.append_committed_step(
                    session_id,
                    outcome,
                    PreviewAllocations::default(),
                    lifecycle_start,
                )
                .await?;
                self.continue_outcome_after_resume(session_id).await?;
            }
            InboundEvent::UserToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let lifecycle_start = self.lifecycle_cursor(session_id)?;
                let outcome = self
                    .application
                    .resume_custom(
                        session_id,
                        tool_use_id,
                        content.clone().unwrap_or_default(),
                        *is_error,
                    )
                    .await?;
                self.append_committed_step(
                    session_id,
                    outcome,
                    PreviewAllocations::default(),
                    lifecycle_start,
                )
                .await?;
                self.continue_outcome_after_resume(session_id).await?;
            }
            InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => {
                let rubric = rubric_text(rubric);
                let progress = self
                    .application
                    .define_outcome(
                        session_id,
                        description,
                        &rubric,
                        max_iterations.unwrap_or(3),
                    )
                    .await?;
                self.project_outcome_drive(session_id, progress).await?;
            }
            InboundEvent::SystemMessage { content } => {
                let text = content_text(content);
                self.application.add_system(session_id, &text).await?;
            }
            InboundEvent::UserInterrupt { session_thread_id } => {
                for thread in self.interrupt_targets(session_id, session_thread_id.as_deref())? {
                    self.application.interrupt(&thread).await?;
                }
            }
        }
        Ok(())
    }

    async fn validate_event_batch(
        &self,
        session_id: &str,
        events: &[InboundEvent],
        deployment_initial: bool,
    ) -> Result<(), StateError> {
        let pending = self
            .application
            .pending_tool(session_id)
            .await
            .map_err(StateError::Run)?;
        let mut pending_resolved = pending.is_none();
        let mut resolution_seen = false;
        for event in events {
            let resolution = match event {
                InboundEvent::UserToolConfirmation { tool_use_id, .. } => {
                    Some((tool_use_id.as_str(), false))
                }
                InboundEvent::UserCustomToolResult {
                    custom_tool_use_id, ..
                } => Some((custom_tool_use_id.as_str(), true)),
                InboundEvent::UserToolResult { tool_use_id, .. } => {
                    Some((tool_use_id.as_str(), true))
                }
                _ => None,
            };
            if let Some((tool_use_id, requires_client_execution)) = resolution {
                let matches_pending = !resolution_seen
                    && pending.as_ref().is_some_and(|pending| {
                        pending.tool_use_id == tool_use_id
                            && pending.client_executed == requires_client_execution
                    });
                if !matches_pending {
                    return Err(StateError::Run(RunError::bad_request(
                        "tool result does not match the pending tool event",
                    )));
                }
                resolution_seen = true;
                pending_resolved = true;
                continue;
            }
            match event {
                InboundEvent::UserInterrupt { session_thread_id } => {
                    // Validate every selector before the first receipt is persisted.
                    // The same canonical resolver is called again during execution;
                    // Session Thread topology cannot change inside this synchronous
                    // batch, so validation and effect address the same target set.
                    self.interrupt_targets(session_id, session_thread_id.as_deref())?;
                }
                InboundEvent::SystemMessage { content } if !(1..=1000).contains(&content.len()) => {
                    return Err(StateError::Run(RunError::bad_request(
                        "system.message content must contain between 1 and 1000 items",
                    )));
                }
                InboundEvent::SystemMessage { .. }
                    if !deployment_initial
                        && !self
                            .application
                            .supports_mid_conversation_system(session_id)
                            .await =>
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "model_does_not_support_mid_conversation_system",
                    )));
                }
                InboundEvent::SystemMessage { .. } if !pending_resolved => {
                    return Err(StateError::Run(RunError::bad_request(
                        "system.message must trail the pending tool result in the same request",
                    )));
                }
                InboundEvent::UserMessage { .. } if !pending_resolved => {
                    return Err(StateError::Run(RunError::bad_request(
                        "pending tool events must be resolved before user.message",
                    )));
                }
                InboundEvent::UserDefineOutcome {
                    max_iterations: Some(iterations),
                    ..
                } if !(1..=20).contains(iterations) => {
                    return Err(StateError::Run(RunError::bad_request(
                        "max_iterations must be between 1 and 20",
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[tracing::instrument(
        name = "sessions.events.send",
        skip_all,
        fields(gen_ai.conversation.id = %session_id)
    )]
    pub async fn send_events(
        &self,
        session_id: &str,
        req: SendEventsRequest,
    ) -> Result<SendEventsResponse, StateError> {
        self.send_event_batch(session_id, req, false).await
    }

    /// One event command for public writes and Deployment initial batches. The
    /// source flag changes only admission of the Deployment-only initial
    /// `system.message`; persistence, processing, projection, and terminal
    /// behavior remain the same implementation.
    async fn send_event_batch(
        &self,
        session_id: &str,
        req: SendEventsRequest,
        deployment_initial: bool,
    ) -> Result<SendEventsResponse, StateError> {
        // Recover the session from durable truth if its in-memory record was lost
        // (a process restart) before resolving the agent — so a resume continues
        // the awaiting run instead of failing closed (ADR-0039).
        self.ensure_session(session_id).await?;
        // An archived session is terminal and read-only: refuse every inbound write
        // (message, resume, interrupt, outcome) with a 409, before touching the
        // runtime — the contract makes an archived session read-only.
        let (agent_id, is_built_in_dream_agent) = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
            if record.session.archived_at.is_some() {
                return Err(StateError::Archived);
            }
            (
                record.agent_id.clone(),
                record.agent_id == awaken_dream_application::BUILT_IN_DREAM_AGENT_ID
                    && record
                        .session
                        .metadata
                        .get("awaken.session.origin")
                        .is_some_and(|origin| origin == "dream"),
            )
        };
        let owner_scope = self
            .owner_scope(session_id)
            .unwrap_or_else(|| super::DEFAULT_SCOPE.to_string());
        if self.application.agent_unavailable(&owner_scope, &agent_id) && !is_built_in_dream_agent {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot admit a new event"
            ))));
        }
        // Batch admission precedes the first receipt/event append. One invalid
        // member therefore cannot leave a partial public history.
        self.validate_event_batch(session_id, &req.events, deployment_initial)
            .await?;

        let mut receipts = Vec::new();
        for inbound in &req.events {
            let drives_turn = matches!(
                inbound,
                InboundEvent::UserToolConfirmation { .. }
                    | InboundEvent::UserCustomToolResult { .. }
                    | InboundEvent::UserToolResult { .. }
                    | InboundEvent::UserDefineOutcome { .. }
            );
            let activity_epoch = if drives_turn {
                let session = self
                    .application
                    .begin_admitted_activity(&agent_id, session_id)
                    .await
                    .map_err(StateError::Run)?;
                let epoch = session.activity_epoch;
                self.refresh_cached_projection(&session)?;
                Some(epoch)
            } else {
                None
            };
            let event_id = self.append_inbound_event(session_id, inbound)?;
            receipts.push(EventReceipt {
                id: event_id.clone(),
                kind: inbound.type_str(),
                processed_at: None,
            });

            let processing = self
                .process_inbound_event(session_id, &agent_id, req.user_profile_id.clone(), inbound)
                .await;
            self.mark_inbound_processed(session_id, &event_id);
            if matches!(
                inbound,
                InboundEvent::UserCustomToolResult { .. }
                    | InboundEvent::UserToolResult { .. }
                    | InboundEvent::UserDefineOutcome { .. }
            ) && let Some(receipt) = receipts.last_mut()
            {
                receipt.processed_at = Some(PROCESSED_AT.to_string());
            }
            if processing.is_err()
                && let Some(record) = self.sessions.lock().unwrap().get_mut(session_id)
            {
                record.project_runtime_status(SessionStatus::Idle);
            }
            if let Some(activity_epoch) = activity_epoch {
                let session = self
                    .application
                    .settle_activity(session_id, activity_epoch)
                    .await
                    .map_err(Self::map_activity_error)?;
                self.refresh_cached_projection(&session)?;
            }
            processing?;
        }
        // Refresh the session's accumulated token usage from the runtime's committed
        // tally, so a subsequent GET /v1/sessions reflects the tokens this turn spent.
        let usage = self
            .application
            .session_usage(session_id)
            .await
            .map_err(StateError::Run)?;
        let budget = self
            .application
            .reconcile_managed_budget_usage(session_id, usage.clone())
            .await
            .map_err(|error| StateError::Run(RunError::unavailable(error.to_string())))?;
        if let Some(record) = self.sessions.lock().unwrap().get_mut(session_id) {
            let mut projected_usage = session_usage_value(usage);
            projected_usage.list_cost =
                budget
                    .session
                    .budget
                    .public_list_cost_minor()
                    .map(|amount| crate::types::MonetaryAmount {
                        amount: amount.to_string(),
                        currency: crate::types::Currency::USD,
                    });
            record.session.usage = projected_usage;
            if budget.reached_now {
                let start = record.events.len();
                record.events.push(Event {
                    id: self.next_event_id(),
                    kind: OutboundKind::SessionStatusIdle {
                        stop_reason: StopReason::BudgetReached,
                    },
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
                record.project_runtime_status(SessionStatus::Idle);
                self.broadcast_committed_from(session_id, record, start);
            }
        }
        Ok(SendEventsResponse { data: receipts })
    }

    fn append_runtime_failure(
        &self,
        session_id: &str,
        error: &awaken_session_contract::RunError,
    ) -> Result<(), StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let start = record.events.len();
        let processed_at = Some(PROCESSED_AT.to_string());
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionStatusRunning {},
            processed_at: processed_at.clone(),
        });
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionError {
                error: SessionError::classify(&error.code, error.message.clone()),
            },
            processed_at: processed_at.clone(),
        });
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::EndTurn,
            },
            processed_at,
        });
        record.project_runtime_status(SessionStatus::Idle);
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// Refresh this process's disposable event projection from the Runtime's one
    /// durable transcript and Run lifecycle feed. A Session cache hit is not proof
    /// that it contains commits accepted through another protocol or Coordinator.
    pub(crate) async fn refresh_committed_events(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        self.ensure_session(session_id).await?;
        // Private Worker realization mutates the same durable Session application
        // without passing through this protocol adapter. Refresh its disposable
        // wire projection before reading runtime events so GET cannot retain a
        // stale preparing/idle status as a parallel lifecycle authority.
        let persisted = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        self.refresh_cached_projection(&persisted)?;
        let initial_cursor = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .ok_or(StateError::NotFound)?
            .projected_lifecycle_cursor;
        const LIFECYCLE_PAGE_SIZE: usize = 256;
        let mut lifecycle_cursor = initial_cursor;
        let mut latest_lifecycle = None;
        loop {
            let page = self
                .application
                .committed_run_lifecycle(session_id, lifecycle_cursor, LIFECYCLE_PAGE_SIZE)
                .await
                .map_err(StateError::Run)?;
            let count = page.events.len();
            for event in page
                .events
                .into_iter()
                .filter(|event| event.thread_id.0 == session_id)
            {
                latest_lifecycle = Some(event);
            }
            if page.next_cursor == lifecycle_cursor || count < LIFECYCLE_PAGE_SIZE {
                lifecycle_cursor = page.next_cursor;
                break;
            }
            lifecycle_cursor = page.next_cursor;
        }
        let pending = self
            .application
            .pending_tool(session_id)
            .await
            .map_err(StateError::Run)?;
        let messages = self
            .application
            .committed_messages(session_id)
            .await
            .map_err(StateError::Run)?;
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let new_messages = messages
            .into_iter()
            .filter(|message| record.projected_message_ids.insert(message.id.0.clone()))
            .collect::<Vec<_>>();
        let (projected_tool_ids, prior_mcp_ids) = record.projected_tool_ids();
        let pending = pending.as_ref();
        let start = record.events.len();
        let terminal = latest_lifecycle.as_ref().filter(|event| {
            matches!(
                event.kind,
                RunLifecycleEventKind::Awaiting
                    | RunLifecycleEventKind::Completed
                    | RunLifecycleEventKind::Failed
                    | RunLifecycleEventKind::Cancelled
            )
        });
        // An Awaiting fact without its exact committed ticket cannot carry the
        // required action id. Keep the cursor before it and retry; never publish an
        // empty requires_action terminal that could supersede the real call.
        let awaiting_ticket_pending = terminal.is_some_and(|event| {
            event.kind == RunLifecycleEventKind::Awaiting && pending.is_none()
        });
        if !awaiting_ticket_pending {
            record.projected_lifecycle_cursor = lifecycle_cursor;
        }
        if latest_lifecycle.as_ref().is_some_and(|event| {
            matches!(
                event.kind,
                RunLifecycleEventKind::Running | RunLifecycleEventKind::Resumed
            )
        }) {
            record.project_runtime_status(SessionStatus::Running);
        } else if terminal.is_some() {
            record.project_runtime_status(SessionStatus::Idle);
        }
        let project_terminal = terminal.filter(|event| {
            !awaiting_ticket_pending && record.projected_terminal_cursors.insert(event.cursor)
        });
        if project_terminal.is_some() {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionStatusRunning {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        let projected = project_messages_with_mcp_ids(
            &new_messages,
            pending,
            &projected_tool_ids,
            prior_mcp_ids.iter().cloned(),
        );
        record
            .events
            .extend(projected.into_iter().map(|event| Event {
                id: event.id.unwrap_or_else(|| self.next_event_id()),
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            }));
        if let Some(terminal) = project_terminal {
            let (projected_tool_ids, prior_mcp_ids) = record.projected_tool_ids();
            if let awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Error(failure),
            ) = &terminal.state
            {
                record.events.push(Event {
                    id: self.next_event_id(),
                    kind: OutboundKind::SessionError {
                        error: SessionError::classify(failure.code(), failure.message()),
                    },
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            record.events.extend(
                project_step(
                    &[],
                    &terminal.state,
                    pending,
                    &projected_tool_ids,
                    prior_mcp_ids,
                )
                .into_iter()
                .map(|event| Event {
                    id: event.id.unwrap_or_else(|| self.next_event_id()),
                    kind: event.kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                }),
            );
        }
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// `GET /v1/sessions/{id}/events` — the session's events in the requested
    /// chronological direction, paged by cursor via the kernel's shared
    /// [`paginate_by_id`]. `cursor` is the id of the last event on the previous
    /// page; an absent/empty cursor starts at that direction's beginning; an
    /// unknown cursor is a caller error (400).
    pub fn list_events(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
        descending: bool,
    ) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        let ordered = if descending {
            record.events.iter().rev().cloned().collect::<Vec<_>>()
        } else {
            record.events.clone()
        };
        let page = paginate_by_id(&ordered, cursor, limit, |e| e.id.as_str())
            .map_err(|_| RunError::bad_request("unknown pagination cursor"))?;
        Ok(ListEventsResponse {
            data: page.items.to_vec(),
            next_page: page.next_page,
            has_more: page.has_more,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecyclePage};
    use awaken_session_contract::{Pending, RunError, SessionRuntime, ToolPermissionDecision};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct LifecycleRuntime {
        messages: Arc<Mutex<Vec<Message>>>,
        lifecycle: Arc<Mutex<Vec<RunLifecycleEvent>>>,
        pending: Arc<Mutex<Option<Pending>>>,
    }

    #[async_trait]
    impl SessionRuntime for LifecycleRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn committed_messages(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
            Ok(self.messages.lock().unwrap().clone())
        }

        async fn committed_run_lifecycle(
            &self,
            _thread: &str,
            cursor: RunLifecycleCursor,
            limit: usize,
        ) -> Result<RunLifecyclePage, RunError> {
            let events = self
                .lifecycle
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.cursor > cursor)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            Ok(RunLifecyclePage {
                next_cursor: events.last().map_or(cursor, |event| event.cursor),
                events,
            })
        }

        async fn pending_tool(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
            Ok(self.pending.lock().unwrap().clone())
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Ok(())
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeDrive, RunError> {
            unreachable!()
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    fn lifecycle(
        cursor: u64,
        thread: &str,
        run_id: &RunId,
        kind: RunLifecycleEventKind,
        state: RunState,
    ) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: RunLifecycleCursor(cursor),
            thread_id: ThreadId(thread.into()),
            run_id: run_id.clone(),
            kind,
            state,
        }
    }

    #[tokio::test]
    async fn committed_lifecycle_closes_cross_protocol_managed_projection_once() {
        // Cause/effect graph: C1=the Session cache is warm; C2=a Run is committed
        // through AI SDK rather than Managed; C3=latest lifecycle is Running;
        // C4=latest lifecycle is Awaiting with its exact pending ticket; C5=latest
        // lifecycle is Ended; C6=the same read refresh repeats; C7=the Managed
        // request already projected that exact Run terminal; C8=the disposable
        // Session cache is cold while the committed ticket remains open; C9=the
        // same Run resumes and reaches a second Awaiting terminal. E1=Managed status is
        // running without a fabricated terminal; E2=Awaiting appends one
        // running→requires_action bracket carrying the custom-call id; E3=Ended
        // appends one running→idle bracket; E4=messages and terminals are not
        // duplicated; E5=the lifecycle cursor fences the projection; E6=cold
        // recovery still classifies the call as client-executed; C10=a concurrent
        // read observes the committed terminal before the local request appends it.
        // E7=each terminal occurrence has exactly one status bracket even when Run
        // id is reused; C11=a durable outcome has no matching lifecycle fact.
        // E8=the exact lifecycle cursor deduplicates either race order; E9=missing
        // durable identity fails closed without appending an unkeyed bracket.
        // Decision table:
        // | Rule | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | C10 | C11 | Effect          |
        // | R1   | T  | T  | F  | F  | F  | F  | F  | F  | F   | F   | E1,E5           |
        // | R2   | T  | F  | T  | F  | T  | F  | F  | F  | F   | F   | E2,E4,E5        |
        // | R3   | T  | F  | F  | T  | T  | F  | F  | F  | F   | F   | E3,E4,E5        |
        // | R4   | T  | F  | F  | T  | T  | T  | F  | F  | F   | F   | E3 once,E4,E8   |
        // | R5   | T  | F  | T  | F  | T  | F  | T  | F  | F   | F   | E2,E4,E5,E6     |
        // | R6   | T  | F  | T  | F  | T  | T  | F  | T  | T   | F   | E2,E4,E5,E7,E8  |
        // | R7   | T  | F  | F  | F  | F  | T  | F  | F  | F   | T   | E4,E9           |
        let runtime = LifecycleRuntime::default();
        let state = ManagedState::new(runtime.clone());
        let request = serde_json::from_value(serde_json::json!({"agent":"coder"})).unwrap();
        let session = state.create_session(request, None).await.unwrap();
        let thread = session.id;
        let first = Message::text(MessageId("cross-user".into()), Role::User, "build");
        runtime.messages.lock().unwrap().push(first);

        let run = RunId("run-cross-1".into());
        runtime.lifecycle.lock().unwrap().push(lifecycle(
            10,
            &thread,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ));
        state.refresh_committed_events(&thread).await.unwrap();
        assert_eq!(
            state.get_session(&thread).unwrap().status,
            SessionStatus::Running,
            "R1"
        );
        let rendered =
            serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
                .unwrap();
        assert!(!rendered.contains("session.status_idle"), "R1");

        let call_id = "call-cross-submit";
        runtime.messages.lock().unwrap().push(Message::new(
            MessageId("cross-tool".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                call_id,
                "design_submit_artifact",
                serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            )],
        ));
        *runtime.pending.lock().unwrap() = Some(Pending {
            tool_use_id: call_id.into(),
            name: "design_submit_artifact".into(),
            input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            client_executed: true,
        });
        runtime.lifecycle.lock().unwrap().push(lifecycle(
            20,
            &thread,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ));
        state.refresh_committed_events(&thread).await.unwrap();
        state.refresh_committed_events(&thread).await.unwrap();
        let rendered =
            serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
                .unwrap();
        assert_eq!(rendered.matches(call_id).count(), 2, "R2");
        assert_eq!(rendered.matches("session.status_idle").count(), 1, "R2/E4");

        let second_run = RunId("run-cross-2".into());
        *runtime.pending.lock().unwrap() = None;
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                30,
                &thread,
                &second_run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                40,
                &thread,
                &second_run,
                RunLifecycleEventKind::Completed,
                RunState::Ended(EndCause::NaturalEnd),
            ),
        ]);
        runtime.messages.lock().unwrap().push(Message::text(
            MessageId("cross-final".into()),
            Role::Assistant,
            "done",
        ));
        state.refresh_committed_events(&thread).await.unwrap();
        state.refresh_committed_events(&thread).await.unwrap();
        let rendered =
            serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
                .unwrap();
        assert_eq!(rendered.matches("session.status_idle").count(), 2, "R3/E4");
        assert_eq!(rendered.matches("done").count(), 1, "R3/E4");

        let local_run = RunId("run-local-3".into());
        let local_message = Message::text(
            MessageId("local-final".into()),
            Role::Assistant,
            "local done",
        );
        state
            .append_step(
                &thread,
                StepOutcome::ended(
                    vec![local_message.clone()],
                    EndCause::NaturalEnd,
                    false,
                    false,
                )
                .with_run_id(local_run.clone()),
                PreviewAllocations::default(),
                Some(RunLifecycleCursor(60)),
            )
            .unwrap();
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                50,
                &thread,
                &local_run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                60,
                &thread,
                &local_run,
                RunLifecycleEventKind::Completed,
                RunState::Ended(EndCause::NaturalEnd),
            ),
        ]);
        runtime.messages.lock().unwrap().push(local_message);
        state.refresh_committed_events(&thread).await.unwrap();
        state.refresh_committed_events(&thread).await.unwrap();
        let rendered =
            serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
                .unwrap();
        assert_eq!(rendered.matches("session.status_idle").count(), 3, "R4/E4");
        assert_eq!(rendered.matches("local done").count(), 1, "R4/E4");

        let resumed_call_id = "call-local-resumed";
        let resumed_message = Message::new(
            MessageId("local-resumed-tool".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                resumed_call_id,
                "design_submit_artifact",
                serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            )],
        );
        let resumed_pending = Pending {
            tool_use_id: resumed_call_id.into(),
            name: "design_submit_artifact".into(),
            input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            client_executed: true,
        };
        *runtime.pending.lock().unwrap() = Some(resumed_pending);
        runtime.messages.lock().unwrap().push(resumed_message);
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                70,
                &thread,
                &local_run,
                RunLifecycleEventKind::Resumed,
                RunState::Running,
            ),
            lifecycle(
                80,
                &thread,
                &local_run,
                RunLifecycleEventKind::Awaiting,
                RunState::Awaiting,
            ),
        ]);
        state.refresh_committed_events(&thread).await.unwrap();
        state
            .append_step(
                &thread,
                StepOutcome::awaiting(
                    Vec::new(),
                    runtime.pending.lock().unwrap().clone(),
                    false,
                    false,
                )
                .with_run_id(local_run.clone()),
                PreviewAllocations::default(),
                Some(RunLifecycleCursor(80)),
            )
            .unwrap();
        state.refresh_committed_events(&thread).await.unwrap();
        let rendered =
            serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
                .unwrap();
        assert_eq!(rendered.matches("session.status_idle").count(), 4, "R6/E7");
        assert_eq!(rendered.matches(resumed_call_id).count(), 2, "R6/E2/E4");

        let cold_run = RunId("run-cold-4".into());
        let cold_call_id = "call-cold-submit";
        *runtime.messages.lock().unwrap() = vec![Message::new(
            MessageId("cold-tool".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                cold_call_id,
                "design_submit_artifact",
                serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            )],
        )];
        *runtime.pending.lock().unwrap() = Some(Pending {
            tool_use_id: cold_call_id.into(),
            name: "design_submit_artifact".into(),
            input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            client_executed: true,
        });
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                90,
                &thread,
                &cold_run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                100,
                &thread,
                &cold_run,
                RunLifecycleEventKind::Awaiting,
                RunState::Awaiting,
            ),
        ]);
        state.sessions.lock().unwrap().remove(&thread);
        state.refresh_committed_events(&thread).await.unwrap();
        state.refresh_committed_events(&thread).await.unwrap();
        let rendered =
            serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
                .unwrap();
        assert_eq!(
            rendered.matches("agent.custom_tool_use").count(),
            1,
            "R5/E6"
        );
        assert!(!rendered.contains("\"type\":\"agent.tool_use\""), "R5/E6");
        assert_eq!(rendered.matches(cold_call_id).count(), 2, "R5/E2/E4");

        let before = state
            .list_events(&thread, None, None, false)
            .unwrap()
            .data
            .len();
        let missing = state
            .append_committed_step(
                &thread,
                StepOutcome::ended(Vec::new(), EndCause::NaturalEnd, false, false)
                    .with_run_id(RunId("run-missing-lifecycle".into())),
                PreviewAllocations::default(),
                RunLifecycleCursor(100),
            )
            .await
            .unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("missing from the lifecycle feed"),
            "R7/E9"
        );
        assert_eq!(
            state
                .list_events(&thread, None, None, false)
                .unwrap()
                .data
                .len(),
            before,
            "R7/E4/E9"
        );
    }
}
