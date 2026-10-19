// @formal entrypoint session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
// @formal entrypoint session_event_batch.exact_retry_replays_and_changed_retry_conflicts
// @formal entrypoint session_event_batch.idempotency_key_appends_at_most_once
// @formal entrypoint tool_result.pending_commit_coordinate_is_exact
// @formal entrypoint tool_result.terminal_run_rejects_late_result
//! Atomic Managed inbound-event validation, lowering, retention, and receipt projection.

use super::*;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_managed_session_events::{OrderEvidence, OrderIndex};
use awaken_session_execution_contract::ProjectionThreadSnapshots;

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
        let (links, child_snapshots) = self.coordinated_projection_prefix(session_id, None).await?;
        let root_snapshot = self.recovery_snapshot(session_id, session_id).await?;
        let persisted_session = self
            .roles
            .roots
            .session(&awaken_session_types::SessionId::from(session_id))
            .await
            .map_err(StateError::from)?;
        let interrupt_only = !events.is_empty()
            && events
                .iter()
                .all(|event| matches!(event, InboundEvent::UserInterrupt { .. }));
        if interrupt_only {
            let mut has_awaiting_run = false;
            for snapshot in root_snapshot.iter().chain(child_snapshots.values()) {
                has_awaiting_run |= Self::current_recovery_run(snapshot)?.is_some_and(|run| {
                    run.state == awaken_agent_contract::agent::run::RunState::Awaiting
                });
            }
            // Preserve the existing accepted no-op for a pure budget pause, but
            // never mistake an Awaiting Run with an isolated/corrupt ticket for
            // that no-op. Interrupt needs topology and lifecycle facts, not reply
            // authority, so it deliberately does not materialize ResumeTickets.
            if !persisted_session.budget.can_admit_model_request()
                && persisted_session.execution == SessionExecutionState::Idle
                && !has_awaiting_run
            {
                return Ok(ValidatedEventBatch {
                    inputs: Vec::new(),
                    drive: ValidatedEventBatchDrive::PreferredBatch,
                });
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
            return Ok(ValidatedEventBatch {
                inputs,
                drive: ValidatedEventBatchDrive::PreferredBatch,
            });
        }
        let lifecycle_events =
            Self::committed_lifecycle_prefix(root_snapshot.iter().chain(child_snapshots.values()))?;
        let candidates = collect_pending_reply_candidates(
            root_snapshot.as_ref(),
            &child_snapshots,
            &lifecycle_events,
        )?;
        let candidates = {
            let historical = awaken_managed_session_events::historical_pending_by_lifecycle(
                &lifecycle_events,
                |thread| {
                    if thread == session_id {
                        root_snapshot.as_ref()
                    } else {
                        child_snapshots.get(thread)
                    }
                },
            )
            .map_err(projection_error)?;
            // Tool-source evidence is built before taking the cache lock. This
            // synchronous scope drops the index, history and guard before any
            // later await, keeping only the original owned candidate vector.
            let index = OrderIndex::new(OrderEvidence {
                updates: None,
                session_id,
                batches: &[],
                closed_intervals: &[],
                budget_reach: &[],
                parent_terminal: Default::default(),
                lifecycle: &lifecycle_events,
                root: root_snapshot.as_ref(),
                children: &child_snapshots,
                links: &links,
                outcomes: &[],
                historical_pending: &historical,
            });
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
            qualify_pending_reply_candidates(record, candidates, &index)
        };
        let tool_reply_phases = Self::tool_reply_projection_phases(&persisted_session, &candidates);
        // Cause/effect table over the one shared phase authority:
        // PendingClient/PendingBuiltin/Unclassified => reject follow-up;
        // Resolving => accept durably but leave execution to the lifecycle
        // selector; Processed => accept and preserve request-local driving.
        // A reply supplied by this batch removes its pending key and retains
        // the established preferred-batch path, which already binds an adjacent
        // System message to that exact reply. No second queue or phase is kept.
        let mut unresolved = std::collections::HashSet::new();
        let mut retained_reply_requires_queue = false;
        for candidate in &candidates {
            match tool_reply_phases
                .get(&candidate.key)
                .copied()
                .unwrap_or(awaken_managed_session_events::RuntimeToolProjectionPhase::Unclassified)
                .followup_admission()
            {
                awaken_managed_session_events::RuntimeToolFollowupAdmission::Reject => {
                    unresolved.insert(candidate.key.clone());
                }
                awaken_managed_session_events::RuntimeToolFollowupAdmission::Queue => {
                    retained_reply_requires_queue = true;
                }
                awaken_managed_session_events::RuntimeToolFollowupAdmission::Ready => {}
            }
        }
        let budget_cap_reached = !persisted_session.budget.can_admit_model_request();
        // A pending committed tool reply is the stronger aggregate state. With
        // no pending reply, a reached shared cap makes interrupt an accepted
        // no-op; it must not create an event/receipt or touch Runtime state.
        let ignore_budget_pause_interrupts = budget_cap_reached
            && persisted_session.execution == SessionExecutionState::Idle
            && candidates.is_empty();
        let mut inputs = Vec::with_capacity(events.len());
        let mut drive = ValidatedEventBatchDrive::PreferredBatch;
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
                        .roles
                        .events
                        .supports_mid_conversation_system(session_id)
                        .await
                        .map_err(StateError::Run)? =>
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
                    if retained_reply_requires_queue {
                        drive = ValidatedEventBatchDrive::LifecycleQueue;
                    }
                    inputs.push(SessionEventInput::UserMessage {
                        content: content.clone(),
                    });
                }
                InboundEvent::SystemMessage { content } => {
                    if retained_reply_requires_queue {
                        drive = ValidatedEventBatchDrive::LifecycleQueue;
                    }
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
        Ok(ValidatedEventBatch { inputs, drive })
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
            .map(|_| awaken_agent_contract::stable_fingerprint(&(&req, &data_subject_id)));
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
                .roles
                .events
                .session_event_batch_idempotency(
                    &awaken_session_types::SessionId::from(session_id),
                    key,
                    fingerprint,
                )
                .await
                .map_err(StateError::Run)?
            {
                awaken_session_application::SessionEventBatchIdempotency::Absent => Ok(None),
                awaken_session_application::SessionEventBatchIdempotency::Exact(batch) => {
                    batch.require_accepted().map_err(|_| {
                        StateError::Run(
                            awaken_session_application_contract::event_admission_closed(),
                        )
                    })?;
                    Ok(Some(batch))
                }
                awaken_session_application::SessionEventBatchIdempotency::Conflict => {
                    Err(StateError::IdempotencyMismatch)
                }
            }
        };
        let receipt_response =
            |batch: &awaken_session_event_contract::SessionEventBatch| SendEventsResponse {
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
        let recovery = if interruption_only {
            self.ensure_session_for_frozen_control(session_id).await
        } else {
            // Recover before resolving the agent so a normal resume continues
            // the awaiting run instead of creating a second execution (ADR-0039).
            self.ensure_session(session_id).await
        };
        // Recovery is part of admission: a concurrent root winner can arrive
        // after the first absent read even when this dependency later fails.
        // The SAME final retry observation below classifies every failure.
        let admission = if let Err(error) = recovery {
            Err(error)
        } else if let Some(batch) = classify_retry().await? {
            return Ok(receipt_response(&batch));
        } else {
            async {
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
                // The owner index is written together with the session record
                // above, so a cached session always has one. Treat a missing
                // owner as an unknown session rather than admitting the event
                // against a compiled-in tenant's Agent availability.
                let owner_scope = self.owner_scope(session_id).ok_or(StateError::NotFound)?;
                if self.roles.events.agent_unavailable(&owner_scope, &agent_id) {
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
                let drive = validated.drive;
                let append = if drive == ValidatedEventBatchDrive::PreferredBatch {
                    self.roles
                        .events
                        .append_and_drive_session_event_batch_idempotent(
                            &awaken_session_types::SessionId::from(session_id),
                            validated.inputs,
                            data_subject_id,
                            traceparent,
                            idempotency_key.clone().zip(request_fingerprint.clone()),
                        )
                        .await
                } else {
                    self.roles
                        .events
                        .append_session_event_batch_idempotent(
                            &awaken_session_types::SessionId::from(session_id),
                            validated.inputs,
                            data_subject_id,
                            traceparent,
                            idempotency_key.clone().zip(request_fingerprint.clone()),
                        )
                        .await
                        .map(|batch| (batch, None))
                };
                append.map(Some).map_err(|error| {
                    if error.code == "idempotency_conflict" {
                        StateError::IdempotencyMismatch
                    } else {
                        StateError::Run(error)
                    }
                })
            }
            .await
        };
        let (accepted, drive_error) = match admission {
            Ok(Some(accepted)) => accepted,
            Ok(None) => return Ok(SendEventsResponse { data: Vec::new() }),
            Err(error) => match classify_retry().await? {
                Some(batch) => return Ok(receipt_response(&batch)),
                None => return Err(error),
            },
        };

        // Retryable dependency failure after the root CAS cannot revoke an
        // acknowledged command. The lifecycle supervisor owns every later retry;
        // this best-effort drive considers reply/interrupt commands or a User
        // command that the canonical selector proves is already the global FIFO
        // head. It cannot race an older User/DefineOutcome or block this request
        // behind that Outcome's external execution.
        if let Some(error) = drive_error {
            tracing::warn!(
                %session_id,
                batch_id = %accepted.batch_id,
                %error,
                "accepted Session Event batch remains queued for lifecycle recovery"
            );
        }
        let persisted = self
            .roles
            .roots
            .session(&awaken_session_types::SessionId::from(session_id))
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
        // The full committed-prefix reducer already owns these same durable
        // inbound receipts. Publishing a receipt directly onto a stale cache
        // could put it before earlier Runtime child facts when those facts are
        // first observed on the next refresh. The accepted root receipt remains
        // the command outcome even if this disposable refresh cannot complete;
        // GET/SSE will retry the one reducer against committed truth.
        if let Err(error) = self.refresh_committed_events(session_id).await {
            tracing::warn!(
                session_id,
                batch_id = %accepted.batch_id,
                %error,
                "accepted Session Event awaits committed projection refresh"
            );
        }
        Ok(SendEventsResponse { data: receipts })
    }
}

/// The existing current-ticket law and exact answered commit, shared by
/// root and child collection. The Pending is held only until source proof.
fn pending_reply_candidate(
    target: SessionThreadTarget,
    snapshot: &RunRecoverySnapshot,
    lifecycle: &[RunLifecycleEvent],
) -> Result<Option<(PendingToolReplyCandidate, Pending)>, StateError> {
    let Some((run_id, correlation_id, pending)) =
        ManagedState::pending_ticket_from_recovery_snapshot(snapshot)?
    else {
        return Ok(None);
    };
    let answered = ManagedState::answered_pending_commit_cursor(
        &snapshot.thread_id,
        &run_id,
        snapshot.store_cursor,
        lifecycle,
    )?;
    Ok(Some((
        PendingToolReplyCandidate::from_pending(
            target,
            snapshot.thread_version,
            answered,
            run_id,
            correlation_id,
            pending.clone(),
        ),
        pending,
    )))
}

fn collect_pending_reply_candidates(
    root: Option<&RunRecoverySnapshot>,
    children: &ProjectionThreadSnapshots,
    lifecycle: &[RunLifecycleEvent],
) -> Result<Vec<(PendingToolReplyCandidate, Pending)>, StateError> {
    let mut candidates = Vec::new();
    let root_prefix = root
        .into_iter()
        .map(|snapshot| (SessionThreadTarget::Primary, snapshot));
    let child_prefixes = children.iter().map(|(thread, snapshot)| {
        (
            SessionThreadTarget::Child(awaken_agent_contract::agent::thread::Id(thread.clone())),
            snapshot,
        )
    });
    for (target, snapshot) in root_prefix.chain(child_prefixes) {
        if let SessionThreadTarget::Child(_) = &target {
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
        }
        if let Some(candidate) = pending_reply_candidate(target, snapshot, lifecycle)? {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// One request-local projection proof over the already-read snapshots.
/// No ticket can supply an audit row that its own Awaiting did not retain.
fn qualify_pending_reply_candidates(
    record: &SessionRecord,
    candidates: Vec<(PendingToolReplyCandidate, Pending)>,
    index: &OrderIndex<'_>,
) -> Vec<PendingToolReplyCandidate> {
    candidates
        .into_iter()
        .map(|(candidate, pending)| {
            bind_pending_reply_projection(record, candidate, &pending, index)
        })
        .collect()
}

fn bind_pending_reply_projection(
    record: &SessionRecord,
    mut candidate: PendingToolReplyCandidate,
    pending: &Pending,
    index: &OrderIndex<'_>,
) -> PendingToolReplyCandidate {
    let session_id = record.session.id.as_str();
    let owner = public_child_thread_id(&candidate.key.target);
    let thread = owner.unwrap_or(session_id);
    let projected = record.projected_tool_index_for(owner);
    let mut pauses = index.lifecycle().iter().filter(|event| {
        event.thread_id.0 == thread
            && event.run_id == candidate.key.expected_run_id
            && event.kind == RunLifecycleEventKind::Awaiting
            && event.source_commit_cursor == candidate.answered_pending_commit_cursor
    });
    // Missing, ambiguous or not-yet-published source stays None; the old
    // Unclassified/follow-up refusal and invalid-reply literal still own it.
    let source = pauses
        .next()
        .filter(|_| pauses.next().is_none())
        .and_then(|pause| index.pending_tool_source(pause, pending).ok());
    candidate.projected_event_id = source.as_ref().and_then(|source| {
        projected
            .event_id_for_source(
                &ManagedState::public_tool_thread_id(session_id, owner),
                &candidate.key.runtime_call_id,
                Some((source.source_id.as_str(), source.call_id.as_str())),
            )
            .ok()
    });
    // Qualified identity owns its family. The existing raw legacy wire
    // path may use only an actually published raw Event's exact family,
    // after the same audit/source proof; resolve_tool_reply still demands
    // one unique pending owner before accepting that older public id.
    candidate.projected_family = source.as_ref().and_then(|_| {
        candidate
            .projected_event_id
            .as_deref()
            .and_then(|id| projected.family_for_event(id))
            .or_else(|| projected.family_for_event(&candidate.key.runtime_call_id))
    });
    candidate
}
