//! Atomic Managed inbound-event validation, lowering, retention, and receipt projection.

use super::*;

impl ManagedState {
    #[cfg(test)]
    pub(super) fn lifecycle_cursor(
        &self,
        session_id: &str,
    ) -> Result<RunLifecycleCursor, StateError> {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .map(|record| record.checkpoint.source.lifecycle_cursor)
            .ok_or(StateError::NotFound)
    }

    pub(super) async fn validate_event_batch(
        &self,
        session_id: &str,
        events: &[InboundEvent],
    ) -> Result<ValidatedEventBatch, StateError> {
        let mut file_documents = 0usize;
        for event in events {
            file_documents = file_documents
                .checked_add(
                    event
                        .validate_content()
                        .map_err(|message| StateError::Run(RunError::bad_request(message)))?,
                )
                .ok_or_else(|| {
                    StateError::Run(RunError::bad_request(
                        "event batch contains too many file-sourced documents",
                    ))
                })?;
        }
        if file_documents > 100 {
            return Err(StateError::Run(RunError::bad_request(
                "event batch supports at most 100 file-sourced document blocks",
            )));
        }
        let system_ordinals = events
            .iter()
            .enumerate()
            .filter_map(|(ordinal, event)| {
                matches!(event, InboundEvent::SystemMessage { .. }).then_some(ordinal)
            })
            .collect::<Vec<_>>();
        if system_ordinals.len() > 1 {
            return Err(StateError::Run(RunError::bad_request(
                "event batch allows at most one system.message",
            )));
        }
        if let Some(&ordinal) = system_ordinals.first()
            && (ordinal + 1 != events.len()
                || ordinal == 0
                || !matches!(
                    events.get(ordinal - 1),
                    Some(
                        InboundEvent::UserMessage { .. }
                            | InboundEvent::UserToolResult { .. }
                            | InboundEvent::UserCustomToolResult { .. }
                    )
                ))
        {
            return Err(StateError::Run(RunError::bad_request(
                "system.message must be final and immediately follow user.message, user.tool_result, or user.custom_tool_result",
            )));
        }
        let (links, child_snapshots) = self.coordinated_projection_prefix(session_id).await?;
        let root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        let persisted_session = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        let interrupt_only = !events.is_empty()
            && events
                .iter()
                .all(|event| matches!(event, InboundEvent::UserInterrupt { .. }));
        if interrupt_only {
            let snapshot_is_awaiting =
                |snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
                    matches!(
                        Self::current_recovery_run(snapshot).1,
                        Some(awaken_agent_contract::agent::run::RunState::Awaiting)
                    )
                };
            let has_awaiting_run = root_snapshot.as_ref().is_some_and(snapshot_is_awaiting)
                || child_snapshots.values().any(snapshot_is_awaiting);
            // Preserve the existing accepted no-op for a pure budget pause, but
            // never mistake an Awaiting Run with an isolated/corrupt ticket for
            // that no-op. Interrupt needs topology and lifecycle facts, not reply
            // authority, so it deliberately does not materialize ResumeTickets.
            if !persisted_session.budget.can_admit_model_request()
                && persisted_session.execution == SessionExecutionState::Idle
                && !has_awaiting_run
            {
                return Ok(ValidatedEventBatch { inputs: Vec::new() });
            }
            let inputs = events
                .iter()
                .map(|event| {
                    let InboundEvent::UserInterrupt { session_thread_id } = event else {
                        unreachable!("interrupt-only batch was classified above")
                    };
                    let targets = Self::interrupt_targets(
                        session_id,
                        session_thread_id.as_deref(),
                        &links,
                        &child_snapshots,
                    )?;
                    Ok(SessionEventInput::Interrupt(SessionEventInterrupt {
                        requested_target: session_thread_id.as_deref().map(|thread_id| {
                            session_thread_target_from_public(session_id, thread_id)
                        }),
                        targets,
                    }))
                })
                .collect::<Result<Vec<_>, StateError>>()?;
            return Ok(ValidatedEventBatch { inputs });
        }
        let lifecycle_events = self.committed_lifecycle_prefix(session_id).await?;
        let mut candidates = Vec::new();
        if let Some(snapshot) = root_snapshot.as_ref()
            && let Some((run_id, correlation_id, pending)) =
                Self::pending_ticket_from_recovery_snapshot(snapshot)?
        {
            let answered_pending_commit_cursor = Self::answered_pending_commit_cursor(
                &snapshot.thread_id,
                &run_id,
                snapshot.store_cursor,
                &lifecycle_events,
            )?;
            candidates.push(PendingToolReplyCandidate::from_pending(
                SessionThreadTarget::Primary,
                snapshot.thread_version,
                answered_pending_commit_cursor,
                run_id,
                correlation_id,
                pending,
            ));
        }
        for (thread_id, snapshot) in &child_snapshots {
            let disposition =
                awaken_agent_contract::thread_disposition_from_committed_state(&snapshot.state)
                    .map_err(|error| {
                        StateError::Run(RunError::internal(format!(
                            "recover coordinated Thread disposition: {error}"
                        )))
                    })?;
            if disposition == awaken_agent_contract::ThreadDisposition::Archived {
                continue;
            }
            if let Some((run_id, correlation_id, pending)) =
                Self::pending_ticket_from_recovery_snapshot(snapshot)?
            {
                let answered_pending_commit_cursor = Self::answered_pending_commit_cursor(
                    &snapshot.thread_id,
                    &run_id,
                    snapshot.store_cursor,
                    &lifecycle_events,
                )?;
                candidates.push(PendingToolReplyCandidate::from_pending(
                    SessionThreadTarget::Child(awaken_agent_contract::agent::thread::Id(
                        thread_id.clone(),
                    )),
                    snapshot.thread_version,
                    answered_pending_commit_cursor,
                    run_id,
                    correlation_id,
                    pending,
                ));
            }
        }
        {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
            for candidate in &mut candidates {
                let projected_event_id = record
                    .projected_tool_index_for(public_child_thread_id(&candidate.key.target))
                    .latest
                    .get(&candidate.key.runtime_call_id)
                    .cloned();
                candidate.projected_family = projected_event_id
                    .as_deref()
                    .and_then(|id| record.events.iter().find(|event| event.id == id))
                    // Upgrade recovery: main emitted the Runtime call id
                    // directly. Its already-projected Event kind remains the
                    // authoritative family even though the id is unqualified.
                    .or_else(|| {
                        record
                            .events
                            .iter()
                            .find(|event| event.id == candidate.key.runtime_call_id)
                    })
                    .and_then(ProjectedToolUseFamily::from_event);
                candidate.projected_event_id = projected_event_id;
            }
        }
        let retained_replies =
            Self::retained_tool_reply_identities(&persisted_session, &candidates);
        let mut unresolved = candidates
            .iter()
            .map(|candidate| candidate.key.clone())
            .filter(|key| !retained_replies.contains(key))
            .collect::<std::collections::HashSet<_>>();
        let budget_cap_reached = !persisted_session.budget.can_admit_model_request();
        // A pending committed tool reply is the stronger aggregate state. With
        // no pending reply, a reached shared cap makes interrupt an accepted
        // no-op; it must not create an event/receipt or touch Runtime state.
        let ignore_budget_pause_interrupts = budget_cap_reached
            && persisted_session.execution == SessionExecutionState::Idle
            && candidates.is_empty();
        let mut inputs = Vec::with_capacity(events.len());
        for event in events {
            let resolution = match event {
                InboundEvent::UserToolConfirmation {
                    tool_use_id,
                    result,
                    deny_message,
                } => {
                    let allow = matches!(result, ConfirmResult::Allow);
                    Some((
                        tool_use_id.as_str(),
                        ToolReplyFamily::Confirmation,
                        SessionEventToolReplyKind::Confirmation {
                            allow,
                            deny_message: deny_message.clone(),
                        },
                    ))
                }
                InboundEvent::UserCustomToolResult {
                    custom_tool_use_id,
                    content,
                    is_error,
                } => Some((
                    custom_tool_use_id.as_str(),
                    ToolReplyFamily::CustomResult,
                    SessionEventToolReplyKind::CustomToolResult {
                        content: content.clone(),
                        is_error: *is_error,
                    },
                )),
                InboundEvent::UserToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((
                    tool_use_id.as_str(),
                    ToolReplyFamily::ToolResult,
                    SessionEventToolReplyKind::ToolResult {
                        content: content.clone(),
                        is_error: *is_error,
                    },
                )),
                _ => None,
            };
            if let Some((public_event_id, family, retained_reply)) = resolution {
                let resolved =
                    Self::resolve_tool_reply(session_id, public_event_id, family, &candidates)?;
                Self::consume_tool_reply_identity(&mut unresolved, &resolved)?;
                inputs.push(SessionEventInput::ToolReply(SessionEventToolReply {
                    tool_request_event_id: public_event_id.to_string(),
                    target: resolved.key.target,
                    expected_run_id: resolved.key.expected_run_id,
                    expected_correlation_id: resolved.key.expected_correlation_id,
                    expected_thread_version: Some(resolved.key.expected_thread_version),
                    answered_pending_commit_cursor: Some(resolved.answered_pending_commit_cursor),
                    runtime_tool_use_id: resolved.key.runtime_call_id,
                    reply: retained_reply,
                }));
                continue;
            }
            match event {
                InboundEvent::UserInterrupt { session_thread_id } => {
                    if ignore_budget_pause_interrupts {
                        continue;
                    }
                    // Resolve and freeze every target before the first receipt is
                    // persisted. Recovery executes only this immutable set; it
                    // never reinterprets a later Session Thread topology.
                    let targets = Self::interrupt_targets(
                        session_id,
                        session_thread_id.as_deref(),
                        &links,
                        &child_snapshots,
                    )?;
                    inputs.push(SessionEventInput::Interrupt(SessionEventInterrupt {
                        requested_target: session_thread_id.as_deref().map(|thread_id| {
                            session_thread_target_from_public(session_id, thread_id)
                        }),
                        targets,
                    }));
                }
                InboundEvent::SystemMessage { content } if !(1..=1000).contains(&content.len()) => {
                    return Err(StateError::Run(RunError::bad_request(
                        "system.message content must contain between 1 and 1000 items",
                    )));
                }
                InboundEvent::SystemMessage { .. }
                    if !self
                        .application
                        .supports_mid_conversation_system(session_id)
                        .await =>
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "model_does_not_support_mid_conversation_system",
                    )));
                }
                InboundEvent::SystemMessage { .. }
                    if Self::unresolved_tool_replies_block_followup(event, &unresolved) =>
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "system.message must trail the pending tool result in the same request",
                    )));
                }
                InboundEvent::UserMessage { .. }
                    if Self::unresolved_tool_replies_block_followup(event, &unresolved) =>
                {
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
                InboundEvent::UserDefineOutcome {
                    description,
                    rubric,
                    ..
                } if description.trim().is_empty() || rubric_text(rubric).trim().is_empty() => {
                    return Err(StateError::Run(RunError::bad_request(
                        "user.define_outcome description and rubric must not be blank",
                    )));
                }
                InboundEvent::UserMessage { content } => {
                    inputs.push(SessionEventInput::UserMessage {
                        content: content.clone(),
                    });
                }
                InboundEvent::SystemMessage { content } => {
                    inputs.push(SessionEventInput::SystemMessage {
                        content: content.clone(),
                    });
                }
                InboundEvent::UserDefineOutcome {
                    description,
                    rubric,
                    max_iterations,
                } => {
                    let rubric = match rubric {
                        OutcomeRubric::Text { content } => SessionOutcomeRubric::Text {
                            content: content.clone(),
                        },
                        OutcomeRubric::File { file_id } => SessionOutcomeRubric::File {
                            file_id: file_id.clone(),
                        },
                    };
                    inputs.push(SessionEventInput::DefineOutcome {
                        description: description.clone(),
                        rubric,
                        max_iterations: *max_iterations,
                    });
                }
                InboundEvent::UserToolConfirmation { .. }
                | InboundEvent::UserCustomToolResult { .. }
                | InboundEvent::UserToolResult { .. } => {
                    unreachable!("tool replies were lowered above")
                }
            }
        }
        Ok(ValidatedEventBatch { inputs })
    }

    pub async fn send_events(
        self: &Arc<Self>,
        session_id: &str,
        req: SendEventsRequest,
    ) -> Result<SendEventsResponse, StateError> {
        Box::pin(self.send_events_attributed(session_id, req, None, None)).await
    }

    /// Send an official Managed event envelope with optional request-grain data
    /// subject attribution supplied by the HTTP adapter. Attribution is context,
    /// not part of the event DTO, so the SDK wire schema remains exact.
    #[tracing::instrument(
        name = "sessions.events.send",
        skip_all,
        fields(gen_ai.conversation.id = %session_id)
    )]
    pub async fn send_events_attributed(
        self: &Arc<Self>,
        session_id: &str,
        req: SendEventsRequest,
        data_subject_id: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<SendEventsResponse, StateError> {
        let traceparent = awaken_observability::current_traceparent();
        Box::pin(self.send_event_batch(
            session_id,
            req,
            data_subject_id,
            traceparent,
            idempotency_key,
        ))
        .await
    }

    /// Validate and atomically retain one complete public Event batch. Durable
    /// reconciliation is owned by the Session lifecycle supervisor; this call
    /// only gives it an opportunistic first drive so receipt-style commands can
    /// expose their committed processed marker without making transport liveness
    /// a recovery dependency.
    pub(super) async fn send_event_batch(
        &self,
        session_id: &str,
        req: SendEventsRequest,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<SendEventsResponse, StateError> {
        let request_fingerprint = idempotency_key
            .as_ref()
            .map(|_| awaken_session_contract::stable_fingerprint(&(&req, &data_subject_id)));
        let replay_coordinate = idempotency_key
            .as_deref()
            .zip(request_fingerprint.as_deref());

        // Cause/effect decision table for transport retry admission:
        // R1 no key => preserve ordinary current-state validation; R2 new key =>
        // validate and append under the Session root CAS; R3 same key+fingerprint
        // => replay the one retained batch before every disposable projection,
        // archived, or pending-tool check; R4 same key+different fingerprint =>
        // 409 and no mutation. A second observation after any admission failure
        // closes the Absent->Exact race. The root receipt is the sole replay
        // authority, so an exact retry neither drives Runtime nor rebuilds or
        // republishes the Managed cache.
        let classify_retry = || async {
            let Some((key, fingerprint)) = replay_coordinate else {
                return Ok(None);
            };
            match self
                .application
                .session_event_batch_idempotency(session_id, key, fingerprint)
                .await
                .map_err(StateError::Run)?
            {
                awaken_session_application::SessionEventBatchIdempotency::Absent => Ok(None),
                awaken_session_application::SessionEventBatchIdempotency::Exact(batch) => {
                    Ok(Some(batch))
                }
                awaken_session_application::SessionEventBatchIdempotency::Conflict => {
                    Err(StateError::IdempotencyMismatch)
                }
            }
        };
        let receipt_response =
            |batch: &awaken_session_contract::SessionEventBatch| SendEventsResponse {
                data: accepted_inbound_receipts(session_id, batch)
                    .into_iter()
                    .map(|projection| projection.event)
                    .collect(),
            };
        if let Some(batch) = classify_retry().await? {
            return Ok(receipt_response(&batch));
        }

        // Cold-admission cause/effect table: C1=every non-empty command is an
        // Interrupt; C2=the frozen Agent publication remains available. E1=C1
        // rebuilds only the disposable frozen-control projection and addresses
        // existing work; E2=!C1+C2 uses interactive recovery; E3=!C1+!C2 fails
        // closed. Constraint: a mixed batch may start/resume work and can never
        // inherit the control-only bypass. Rules A1=C1=>E1; A2=!C1+C2=>E2;
        // A3=!C1+!C2=>E3. Exact replays returned above never enter this cache
        // recovery path.
        let interruption_only = !req.events.is_empty()
            && req
                .events
                .iter()
                .all(|event| matches!(event, InboundEvent::UserInterrupt { .. }));
        if interruption_only {
            self.ensure_session_for_frozen_control(session_id).await?;
        } else {
            // Recover before resolving the agent so a normal resume continues
            // the awaiting run instead of creating a second execution (ADR-0039).
            let mut retry = 0_u8;
            loop {
                match self.ensure_session(session_id).await {
                    Ok(()) => break,
                    Err(error)
                        if retry < 2 && error.is_transient_sqlite_initialization_failure() =>
                    {
                        retry += 1;
                        tracing::warn!(
                            %session_id,
                            retry,
                            "retrying transient local Session SQLite initialization"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(25 * u64::from(retry)))
                            .await;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let accepted = if let Some(batch) = classify_retry().await? {
            return Ok(receipt_response(&batch));
        } else {
            let admission = async {
                // An archived session is terminal and read-only: refuse every inbound write
                // (message, resume, interrupt, outcome) with a 409, before touching the
                // runtime — the contract makes an archived session read-only.
                let (agent_id, inference_geo) = {
                    let sessions = self.sessions.lock().unwrap();
                    let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
                    if record.session.archived_at.is_some() {
                        return Err(StateError::Archived);
                    }
                    (
                        record.agent_id.clone(),
                        record.session.agent.model.inference_geo,
                    )
                };
                let owner_scope = self
                    .owner_scope(session_id)
                    .unwrap_or_else(|| super::DEFAULT_SCOPE.to_string());
                if self.application.agent_unavailable(&owner_scope, &agent_id) {
                    return Err(StateError::Run(RunError::bad_request(format!(
                        "agent_unavailable: agent `{agent_id}` cannot admit a new event"
                    ))));
                }
                let starts_run = req.events.iter().any(|event| {
                    matches!(
                        event,
                        InboundEvent::UserMessage { .. }
                            | InboundEvent::UserToolConfirmation { .. }
                            | InboundEvent::UserCustomToolResult { .. }
                            | InboundEvent::UserToolResult { .. }
                            | InboundEvent::UserDefineOutcome { .. }
                    )
                });
                if starts_run {
                    self.authorize_inference_geo(
                        &owner_scope,
                        inference_geo,
                        crate::InferenceGeoCheckpoint::Run,
                    )
                    .await?;
                }
                // Batch admission precedes the first receipt/event append. One invalid
                // member therefore cannot leave a partial public history.
                let validated = self.validate_event_batch(session_id, &req.events).await?;
                if validated.inputs.is_empty() {
                    return Ok(None);
                }
                self.application
                    .append_session_event_batch_idempotent(
                        session_id,
                        validated.inputs,
                        data_subject_id,
                        traceparent,
                        idempotency_key.clone().zip(request_fingerprint.clone()),
                    )
                    .await
                    .map(Some)
                    .map_err(|error| {
                        if error.code == "idempotency_conflict" {
                            StateError::IdempotencyMismatch
                        } else {
                            StateError::Run(error)
                        }
                    })
            }
            .await;
            match admission {
                Ok(Some(batch)) => batch,
                Ok(None) => return Ok(SendEventsResponse { data: Vec::new() }),
                Err(error) => match classify_retry().await? {
                    Some(batch) => return Ok(receipt_response(&batch)),
                    None => return Err(error),
                },
            }
        };

        // Retryable dependency failure after the root CAS cannot revoke an
        // acknowledged command. The lifecycle supervisor owns every later retry;
        // this best-effort drive considers only reply/interrupt commands in the
        // just-admitted batch. It cannot race an older DefineOutcome and block
        // this request behind that Outcome's external execution.
        if let Err(error) = self
            .application
            .drive_session_event_batches(session_id, Some(&accepted.batch_id))
            .await
        {
            tracing::warn!(
                %session_id,
                batch_id = %accepted.batch_id,
                %error,
                "accepted Session Event batch remains queued for lifecycle recovery"
            );
        }
        let persisted = self
            .application
            .session(session_id)
            .await
            .map_err(StateError::from)?;
        self.publish_persisted_session(&persisted)?;
        let accepted = persisted
            .event_batches
            .iter()
            .find(|batch| batch.batch_id == accepted.batch_id)
            .ok_or_else(|| {
                StateError::Run(RunError::internal(
                    "accepted Session Event batch disappeared from the root",
                ))
            })?;
        let receipt_projections = accepted_inbound_receipts(session_id, accepted);
        let receipts = receipt_projections
            .iter()
            .map(|projection| projection.event.clone())
            .collect::<Vec<_>>();
        self.publish_projection_update(session_id, |candidate| {
            merge_durable_inbound_projections(candidate, receipt_projections.clone());
            Ok(())
        })?;
        Ok(SendEventsResponse { data: receipts })
    }
}
