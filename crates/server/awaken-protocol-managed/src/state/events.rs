//! Event driving for [`ManagedState`]: projecting committed turns/outcomes,
//! the live-inbox surface, and `send_events`/`list_events`.

use super::*;
use awaken_agent_contract::agent::delegation::DelegationStatus;

struct DelegateCall {
    run_id: String,
    agent_name: String,
    sent: Vec<ContentBlock>,
    received: Vec<ContentBlock>,
    status: DelegationStatus,
}

impl ManagedState {
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
            record.session.status = "running";
        }
        let state = Arc::clone(self);
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            if let Err(error) = state
                .send_events(
                    &session_id,
                    SendEventsRequest {
                        events,
                        user_profile_id: None,
                    },
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

    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    /// Project a committed turn into events and append them. Preview allocations
    /// carry the ids minted for message and thinking starts; their corresponding
    /// buffered events reuse them so a client reconciles by id.
    fn append_step(
        &self,
        session_id: &str,
        outcome: StepOutcome,
        mut preview_ids: PreviewAllocations,
    ) -> Result<(), StateError> {
        let pending = outcome
            .pending()
            .map(|p| (p.tool_use_id.as_str(), p.client_executed));
        let delegated_runs = outcome.delegated_runs().to_vec();
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let prior_mcp_ids: Vec<String> = record
            .events
            .iter()
            .filter_map(|event| match event.kind {
                OutboundKind::AgentMcpToolUse { .. } => Some(event.id.clone()),
                _ => None,
            })
            .collect();
        // A concurrent GET may already have observed this process's committed
        // messages through the shared transcript and projected them. Message ids
        // are the canonical dedupe key; status brackets remain request-local.
        let new_messages = outcome
            .messages
            .iter()
            .filter(|message| record.projected_message_ids.insert(message.id.0.clone()))
            .cloned()
            .collect::<Vec<_>>();
        let projected = project_step(&new_messages, outcome.state(), pending, prior_mcp_ids);
        // Everything appended from here is republished on the live broadcast at the end.
        let start = record.events.len();
        // Each processing segment is bracketed `running` … `idle`; the running
        // marker leads before any fold or message.
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionStatusRunning {},
            processed_at: Some(PROCESSED_AT.to_string()),
        });
        // A transparent transient-retry recovery surfaces as `session.status_rescheduled`
        // between the running marker and the turn's output, so a client observes that
        // the runtime auto-recovered rather than seeing an unexplained pause.
        if outcome.rescheduled {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionStatusRescheduled {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // Compaction ran at BeforeInference, so its marker precedes the turn's
        // message events. `true` ⇒ this terminal step folded (emit-once upstream).
        if outcome.compacted {
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
        if let Some(failure) = outcome.failure() {
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
        record.session.status = "idle";
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
        let projected_message_ids = report
            .iterations
            .iter()
            .flat_map(|round| round.messages.iter().map(|message| message.id.0.clone()))
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
            for round in report.iterations {
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
        record.projected_message_ids.extend(projected_message_ids);
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
        record.session.status = "idle";
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// `POST /v1/sessions/{id}/events`. Mints a receipt per inbound event and acts
    /// on `user.message` (run a turn) and `user.tool_confirmation` (resume a
    /// awaiting run), appending the projected events.
    /// Resolve `session_id` (rehydrating from durable truth after a restart,
    /// like `send_events`) and fail closed when it names no session.
    async fn require_session(&self, session_id: &str) -> Result<(), StateError> {
        self.ensure_session(session_id).await?;
        let sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(session_id) {
            Ok(())
        } else {
            Err(StateError::NotFound)
        }
    }

    /// `GET /v1/sessions/:id/live-inbox` — the in-flight turn's editable queue.
    pub async fn live_inbox_snapshot(
        &self,
        session_id: &str,
    ) -> Result<LiveInboxSnapshot, StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_snapshot(session_id).await)
    }

    /// `POST /v1/sessions/:id/live-inbox` — queue a message for the in-flight turn.
    pub async fn live_inbox_queue(
        &self,
        session_id: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_queue(session_id, content).await?)
    }

    /// `DELETE /v1/sessions/:id/live-inbox/:msg` — withdraw a queued message.
    pub async fn live_inbox_remove(&self, session_id: &str, id: u64) -> Result<(), StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_remove(session_id, id).await?)
    }

    /// `PUT /v1/sessions/:id/live-inbox/:msg` — replace a queued message's content.
    pub async fn live_inbox_replace(
        &self,
        session_id: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), StateError> {
        self.require_session(session_id).await?;
        Ok(self
            .runtime
            .live_inbox_replace(session_id, id, content)
            .await?)
    }

    /// `PUT /v1/sessions/:id/live-inbox/order` — reorder the queue (full permutation).
    pub async fn live_inbox_reorder(
        &self,
        session_id: &str,
        order: Vec<u64>,
    ) -> Result<(), StateError> {
        self.require_session(session_id).await?;
        Ok(self.runtime.live_inbox_reorder(session_id, order).await?)
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
                if let Some(model) = model {
                    self.runtime.rebind_model(session_id, model).await?;
                }
                let sink = Arc::new(PreviewSink::new(
                    self.live_sender(session_id),
                    self.event_seq.clone(),
                ));
                let outcome = self
                    .runtime
                    .run_streaming_attributed(
                        agent_id,
                        session_id,
                        content.clone(),
                        user_profile_id,
                        sink.clone(),
                    )
                    .await;
                self.persist_session_environment_binding(session_id).await?;
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.append_runtime_failure(session_id, &error)?;
                        return Err(StateError::Run(error));
                    }
                };
                self.append_step(session_id, outcome, sink.take_allocations())?;
            }
            InboundEvent::UserToolConfirmation {
                tool_use_id,
                result,
                deny_message,
            } => {
                let decision = ToolPermissionDecision {
                    allow: matches!(result, ConfirmResult::Allow),
                    note: deny_message.clone(),
                };
                let outcome = self
                    .runtime
                    .resume(session_id, tool_use_id, decision)
                    .await?;
                self.append_step(session_id, outcome, PreviewAllocations::default())?;
            }
            InboundEvent::UserCustomToolResult {
                custom_tool_use_id,
                content,
                is_error,
            } => {
                let text = content.as_deref().map(content_text).unwrap_or_default();
                let outcome = self
                    .runtime
                    .resume_custom(session_id, custom_tool_use_id, &text, *is_error)
                    .await?;
                self.append_step(session_id, outcome, PreviewAllocations::default())?;
            }
            InboundEvent::UserToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let text = content.as_deref().map(content_text).unwrap_or_default();
                let outcome = self
                    .runtime
                    .resume_custom(session_id, tool_use_id, &text, *is_error)
                    .await?;
                self.append_step(session_id, outcome, PreviewAllocations::default())?;
            }
            InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => {
                let rubric = rubric_text(rubric);
                let report = self
                    .runtime
                    .define_outcome(
                        session_id,
                        description,
                        &rubric,
                        max_iterations.unwrap_or(3),
                    )
                    .await?;
                self.append_outcome(session_id, report)?;
            }
            InboundEvent::SystemMessage { content } => {
                let text = content_text(content);
                self.runtime.add_system(session_id, &text).await?;
            }
            InboundEvent::UserInterrupt { session_thread_id } => {
                for thread in self.interrupt_targets(session_id, session_thread_id.as_deref())? {
                    self.runtime.interrupt(&thread).await?;
                }
            }
        }
        Ok(())
    }

    async fn validate_event_batch(
        &self,
        session_id: &str,
        events: &[InboundEvent],
    ) -> Result<(), StateError> {
        let pending = self.runtime.pending_tool(session_id).await;
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
                    if !self
                        .runtime
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
                record.agent_id == crate::dream::BUILT_IN_DREAM_AGENT_ID
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
        if self
            .config_source
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(&owner_scope, &agent_id))
            && !is_built_in_dream_agent
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot admit a new event"
            ))));
        }
        // Batch admission precedes the first receipt/event append. One invalid
        // member therefore cannot leave a partial public history.
        self.validate_event_batch(session_id, &req.events).await?;

        let mut receipts = Vec::new();
        for inbound in &req.events {
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
                record.session.status = "idle";
            }
            processing?;
            // A first execution may have materialized the Session-owned sandbox.
            // Commit its opaque identity before the API acknowledges this event,
            // so a later process adopts instead of provisioning over its workspace.
            self.persist_session_environment_binding(session_id).await?;
        }
        // Refresh the session's accumulated token usage from the runtime's committed
        // tally, so a subsequent GET /v1/sessions reflects the tokens this turn spent.
        let usage = self.runtime.session_usage(session_id).await;
        if let Some(record) = self.sessions.lock().unwrap().get_mut(session_id) {
            record.session.usage = session_usage_value(usage);
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
        record.session.status = "idle";
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// Refresh this process's disposable event projection from the Runtime's one
    /// durable transcript. A Session cache hit is not proof that it contains
    /// commits accepted through another Coordinator replica.
    pub(crate) async fn refresh_committed_events(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        self.ensure_session(session_id).await?;
        let pending = self.runtime.pending_tool(session_id).await;
        let messages = self.runtime.committed_messages(session_id).await;
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let new_messages = messages
            .into_iter()
            .filter(|message| record.projected_message_ids.insert(message.id.0.clone()))
            .collect::<Vec<_>>();
        if new_messages.is_empty() {
            return Ok(());
        }
        let prior_mcp_ids = record.events.iter().filter_map(|event| match event.kind {
            OutboundKind::AgentMcpToolUse { .. } => Some(event.id.clone()),
            _ => None,
        });
        let pending = pending
            .as_ref()
            .map(|pending| (pending.tool_use_id.as_str(), pending.client_executed));
        let projected = project_messages_with_mcp_ids(&new_messages, pending, prior_mcp_ids);
        let start = record.events.len();
        record
            .events
            .extend(projected.into_iter().map(|event| Event {
                id: event.id.unwrap_or_else(|| self.next_event_id()),
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            }));
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// `GET /v1/sessions/{id}/events` — the session's events, oldest-first, paged
    /// by cursor via the kernel's shared [`paginate_by_id`]. `cursor` is the id of
    /// the last event on the previous page; an absent/empty cursor starts at the
    /// beginning; an unknown cursor is a caller error (400).
    pub fn list_events(
        &self,
        session_id: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        let page = paginate_by_id(&record.events, cursor, limit, |e| e.id.as_str())
            .map_err(|_| RunError::bad_request("unknown pagination cursor"))?;
        Ok(ListEventsResponse {
            data: page.items.to_vec(),
            next_page: page.next_page,
            has_more: page.has_more,
        })
    }
}
