//! Event driving for [`ManagedState`]: projecting committed turns/outcomes,
//! the live-inbox surface, and `send_events`/`list_events`.

use super::*;

impl ManagedState {
    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    /// Project a committed turn into events and append them. `preview_ids` are the
    /// ids the turn's [`PreviewSink`] minted for each previewed `agent.message`, in
    /// order — the buffered `agent.message` events reuse them so a client reconciles
    /// preview → committed by id (empty for resume/non-streamed paths). Every newly
    /// appended event is republished on the live broadcast.
    fn append_step(
        &self,
        session_id: &str,
        outcome: StepOutcome,
        preview_ids: Vec<String>,
    ) -> Result<(), StateError> {
        let pending = outcome
            .pending
            .as_ref()
            .map(|p| (p.tool_use_id.as_str(), p.client_executed));
        let projected = project_step(&outcome.messages, outcome.stop, pending);
        // Delegation runs inline as an `agent_run` tool call; each one spawns a
        // subagent child thread (ADR-0047 D4). Collect each delegate's name, the
        // input it was sent, and the reply it returned (matched by tool-use id),
        // before the projected events are consumed.
        struct DelegateCall {
            agent_name: String,
            tool_use_id: Option<String>,
            sent: Vec<ContentBlock>,
            received: Vec<ContentBlock>,
        }
        let mut delegates: Vec<DelegateCall> = projected
            .iter()
            .filter_map(|e| match &e.kind {
                OutboundKind::AgentToolUse { name, input, .. } if name == "agent_run" => {
                    Some(DelegateCall {
                        agent_name: input
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        tool_use_id: e.id.clone(),
                        sent: vec![ContentBlock::text(
                            input
                                .get("input")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default(),
                        )],
                        received: Vec::new(),
                    })
                }
                _ => None,
            })
            .collect();
        for e in &projected {
            if let OutboundKind::AgentToolResult {
                tool_use_id,
                content,
                ..
            } = &e.kind
                && let Some(d) = delegates
                    .iter_mut()
                    .find(|d| d.tool_use_id.as_deref() == Some(tool_use_id.as_str()))
            {
                d.received = content.clone();
            }
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        // Everything appended from here is republished on the live broadcast at the end.
        let start = record.events.len();
        let mut preview_ids = preview_ids.into_iter();
        // Each processing segment is bracketed `running` … `idle`; the running
        // marker leads before any fold or message.
        record.events.push(Event {
            id: self.next_event_id(),
            kind: OutboundKind::SessionStatusRunning {},
            processed_at: Some(PROCESSED_AT.to_string()),
        });
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
        if let Some(failure) = &outcome.failure {
            record.events.push(Event {
                id: self.next_event_id(),
                kind: OutboundKind::SessionError {
                    error: SessionError::classify(&failure.code, failure.message.clone()),
                },
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        for event in projected {
            // An `agent.message` (minted with no id) reuses the id its live preview
            // announced, so `event_start.event.id == agent.message.id` and the SDK
            // discards the accumulated preview on the buffered event. Other events, and
            // any message beyond the previewed count, mint a fresh id as before.
            let id = event.id.unwrap_or_else(|| {
                if matches!(event.kind, OutboundKind::AgentMessage { .. }) {
                    preview_ids.next().unwrap_or_else(|| self.next_event_id())
                } else {
                    self.next_event_id()
                }
            });
            record.events.push(Event {
                id,
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        // Register a child thread per delegate call and project its full inline
        // lifecycle: `created` → `status_running` → the input sent to the delegate →
        // the reply received → `status_idle`. The child thread stays enumerable via
        // the thread API; its terminal `status_terminated` is the archive transition
        // (see `threads.rs`), not the end of this one-shot delegation.
        for d in delegates {
            let thread_id = format!("{}:thread:{}", session_id, record.child_threads.len());
            record.child_threads.push(Self::child_thread(
                &record.session,
                &thread_id,
                &d.agent_name,
            ));
            let name = d.agent_name;
            for kind in [
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
                    to_agent_name: name.clone(),
                    content: d.sent,
                },
                OutboundKind::AgentThreadMessageReceived {
                    from_session_thread_id: thread_id.clone(),
                    from_agent_name: name.clone(),
                    content: d.received,
                },
                OutboundKind::SessionThreadStatusIdle {
                    session_thread_id: thread_id.clone(),
                    agent_name: name.clone(),
                    stop_reason: StopReason::EndTurn,
                },
            ] {
                record.events.push(Event {
                    id: self.next_event_id(),
                    kind,
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
        }
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
        let mut evaluations: Vec<serde_json::Value> = Vec::new();
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
        // The session object carries the running list of evaluations that have graded
        // it, so a `GET /v1/sessions/{id}` reflects the outcomes that ran, not [].
        record.session.outcome_evaluations.extend(evaluations);
        self.broadcast_committed_from(session_id, record, start);
        Ok(())
    }

    /// `POST /v1/sessions/{id}/events`. Mints a receipt per inbound event and acts
    /// on `user.message` (run a turn) and `user.tool_confirmation` (resume a
    /// parked run), appending the projected events.
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
        // the parked run instead of failing closed (ADR-0039).
        self.ensure_session(session_id).await?;
        // An archived session is terminal and read-only: refuse every inbound write
        // (message, resume, interrupt, outcome) with a 409, before touching the
        // runtime — the contract makes an archived session read-only.
        let agent_id = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
            if record.session.archived_at.is_some() {
                return Err(StateError::Archived);
            }
            record.agent_id.clone()
        };

        let mut receipts = Vec::new();
        for inbound in &req.events {
            receipts.push(EventReceipt {
                id: self.next_event_id(),
                kind: inbound.type_str(),
                processed_at: None,
            });

            match inbound {
                InboundEvent::UserMessage { content, model, .. } => {
                    // R5: a per-turn model override rebinds the thread before the turn.
                    if let Some(model) = model {
                        self.runtime.rebind_model(session_id, model).await?;
                    }
                    // Install a preview sink so a concurrently-open SSE stream (opted
                    // in via `event_deltas[]`) sees this turn's `agent.message` text as
                    // live `event_start`/`event_delta` frames. The committed outcome is
                    // identical; the sink only mirrors in-flight text and hands back the
                    // ids it minted so the buffered messages reuse them.
                    let sink = Arc::new(PreviewSink::new(
                        self.live_sender(session_id),
                        self.event_seq.clone(),
                    ));
                    let outcome = self
                        .runtime
                        .run_streaming(&agent_id, session_id, content.clone(), sink.clone())
                        .await?;
                    self.append_step(session_id, outcome, sink.take_allocated_ids())?;
                }
                InboundEvent::UserToolConfirmation {
                    tool_use_id,
                    result,
                    deny_message,
                } => {
                    let decision = Decision {
                        allow: matches!(result, ConfirmResult::Allow),
                        note: deny_message.clone(),
                    };
                    let outcome = self
                        .runtime
                        .resume(session_id, tool_use_id, decision)
                        .await?;
                    self.append_step(session_id, outcome, Vec::new())?;
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
                    self.append_step(session_id, outcome, Vec::new())?;
                }
                // The generic `user.tool_result`: a client-provided result for a
                // parked tool, keyed by `tool_use_id`. Same delivery as a custom
                // tool result (the id addresses the parked tool either way).
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
                    self.append_step(session_id, outcome, Vec::new())?;
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
                // `user.interrupt`: cancel the run in flight on this thread (from a
                // concurrent request), so an in-progress outcome ends `interrupted`.
                InboundEvent::UserInterrupt { .. } => {
                    self.runtime.interrupt(session_id).await?;
                }
            }
        }
        // Refresh the session's accumulated token usage from the runtime's committed
        // tally, so a subsequent GET /v1/sessions reflects the tokens this turn spent.
        let usage = self.runtime.session_usage(session_id).await;
        if let Some(record) = self.sessions.lock().unwrap().get_mut(session_id) {
            record.session.usage = session_usage_value(usage);
        }
        Ok(SendEventsResponse { data: receipts })
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
