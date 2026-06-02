//! Mailbox submission paths: `submit`, `submit_background`,
//! `submit_live_then_queue`, and the run-prep helpers used by all three.
//!
//! All methods stay on `Mailbox` via an additional `impl` block. Visibility
//! is widened to `pub(super)` only where a sibling submodule needs cross-file
//! access — public API surface remains unchanged.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use tokio::sync::mpsc;

use awaken_runtime::{RegistryResolutionScope, ResolutionPolicy, RunActivation};
use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::mailbox::RunDispatch;
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::run::{RunInput, RunInputSnapshot, RunKind};
use awaken_server_contract::contract::storage::{MessageSeqRange, RunRecord, RunRequestSnapshot};
use awaken_server_contract::contract::tool_intercept::RunMode;
use awaken_server_contract::now_ms;

use crate::transport::channel_sink::ReconnectableEventSink;

use super::{
    ACTIVE_RUN_CONFLICT_MESSAGE, INLINE_CLAIM_GUARD_MS, LegacyRunRequestSnapshotAdapter,
    LegacyRunSnapshotExtras, Mailbox, MailboxDispatchStatus, MailboxError, MailboxSubmitResult,
    MailboxWorkerStatus, ThreadContext, build_run_input, legacy_input_snapshot, lock_thread_append,
    normalize_mailbox_run_mode, normalize_message_ids, record_mailbox_operation_result,
    result_label, run_activation_snapshot, validate_run_inputs,
};

impl Mailbox {
    // ── Submission ───────────────────────────────────────────────────

    async fn enqueue_dispatch_for_request(
        &self,
        request: &RunActivation,
        dispatch: &RunDispatch,
    ) -> Result<(), MailboxError> {
        let result = self.enqueue_dispatch_with_metrics(dispatch).await;
        if let Err(error) = &result
            && !request.control.seeded_decisions.is_empty()
        {
            self.record_mailbox_resume_failed(
                dispatch,
                &request.control.seeded_decisions,
                "enqueue_failed",
                error,
            )
            .await;
        }
        result?;
        Ok(())
    }

    /// Submit a run for streaming. Returns event receiver immediately.
    ///
    /// The dispatch is persisted (WAL), then claimed inline by this process.
    /// The caller wires `event_rx` to their transport (SSE, WebSocket, etc).
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id()))]
    pub async fn submit(
        self: &Arc<Self>,
        mut request: RunActivation,
    ) -> Result<(MailboxSubmitResult, mpsc::Receiver<AgentEvent>), MailboxError> {
        normalize_mailbox_run_mode(&mut request, false);
        let (thread_id, messages) = validate_run_inputs(
            request.thread_id().to_owned(),
            request.messages().to_vec(),
            !request.control.seeded_decisions.is_empty(),
        )?;

        // Preflight before any interrupt/cancel side effect: if a barrier ahead
        // in pending blocks this foreground interrupt, the later freeze would
        // select nothing and fail Internal — but only after the active run was
        // already cancelled. Surface it as a clean business error first, leaving
        // the active run untouched (ADR-0042 D6).
        if request.trace.run_mode == RunMode::Foreground {
            self.preflight_foreground_pending(&thread_id).await?;
        }

        // Step 1: Interrupt — bump dispatch epoch, supersede stale queued dispatches.
        let now = now_ms();
        let interrupt_start = Instant::now();
        match self.store.interrupt_detailed(&thread_id, now).await {
            Ok(interrupt) => {
                record_mailbox_operation_result("interrupt", "ok", interrupt_start);
                crate::metrics::inc_mailbox_operation_by(
                    "supersede",
                    "ok",
                    interrupt.superseded_count as u64,
                );
                self.refresh_dispatch_depth_metrics().await;
                for superseded_dispatch in &interrupt.superseded_dispatches {
                    self.mark_superseded_dispatch_run_cancelled(
                        superseded_dispatch,
                        "queued dispatch superseded by foreground submit",
                    )
                    .await;
                }
                // Step 2: Cancel active runtime run if the interrupt found one.
                if let Some(active_dispatch) = interrupt.active_dispatch.as_ref() {
                    let cancelled = self
                        .cancel_active_dispatch(&thread_id, active_dispatch, true)
                        .await?;
                    if !cancelled {
                        return Err(MailboxError::Validation(ACTIVE_RUN_CONFLICT_MESSAGE.into()));
                    }
                    self.record_mailbox_dispatch_event("RunInterrupted", active_dispatch)
                        .await;
                    tracing::info!(
                        thread_id = %thread_id,
                        superseded = interrupt.superseded_count,
                        "interrupted thread for new submission"
                    );
                }
            }
            Err(e) => {
                record_mailbox_operation_result("interrupt", "error", interrupt_start);
                tracing::warn!(thread_id = %thread_id, error = %e, "interrupt failed, falling back to cancel");
                if !self.executor.cancel_and_wait_by_thread(&thread_id).await {
                    return Err(MailboxError::Validation(ACTIVE_RUN_CONFLICT_MESSAGE.into()));
                }
            }
        }

        self.ensure_dispatch_id_hint(&mut request);
        let run_id = self
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await?;
        let dispatch = self.build_dispatch(&request, &thread_id)?;
        let dispatch_id = dispatch.dispatch_id().clone();
        let thread_id = dispatch.thread_id().clone();

        // WAL: persist after the prepared checkpoint; startup recovery reconciles
        // the crash window. available_at is set slightly ahead so the sweep does not
        // grab the dispatch during the inline claim window (reclaimed after the guard).
        let wal_dispatch = dispatch.with_available_at(now_ms() + INLINE_CLAIM_GUARD_MS);
        self.enqueue_dispatch_for_request(&request, &wal_dispatch)
            .await?;
        self.record_mailbox_dispatch_event("RunQueued", &wal_dispatch)
            .await;

        // Inline claim.
        let now = now_ms();
        let claim_start = Instant::now();
        let claimed_result = self
            .store
            .claim_dispatch(&dispatch_id, &self.consumer_id, self.config.lease_ms, now)
            .await;
        let claim_result_label = match &claimed_result {
            Ok(Some(_)) => "ok",
            Ok(None) => "empty",
            Err(_) => "error",
        };
        record_mailbox_operation_result("claim_dispatch", claim_result_label, claim_start);
        let claimed = claimed_result?;
        self.refresh_dispatch_depth_metrics().await;

        let (event_tx, event_rx) = mpsc::channel(Self::EVENT_CHANNEL_CAPACITY);

        if let Some(claimed_dispatch) = claimed {
            let claim_token = claimed_dispatch
                .claim_token()
                .map(str::to_string)
                .ok_or_else(|| {
                    MailboxError::Internal(format!(
                        "claimed dispatch '{}' is missing claim token",
                        claimed_dispatch.dispatch_id()
                    ))
                })?;

            // Shared flag: set by the event sink when a tool call is suspended.
            let suspended = Arc::new(AtomicBool::new(false));

            // Start lease renewal.
            let lease_handle = self.spawn_lease_renewal(
                dispatch_id.clone(),
                claim_token.clone(),
                thread_id.clone(),
                Arc::clone(&suspended),
            );

            // Create reconnectable sink for SSE reconnection on resume.
            let reconnectable_sink = Arc::new(ReconnectableEventSink::new(event_tx.clone()));

            // Pre-warm thread context cache.
            let thread_ctx = match ThreadContext::load(self.run_store.as_ref(), &thread_id).await {
                Ok(ctx) => Some(ctx),
                Err(e) => {
                    tracing::warn!(thread_id, error = %e, "failed to pre-warm thread context");
                    None
                }
            };

            // Update worker state.
            let worker = self.get_or_create_worker(&thread_id).await;
            {
                let mut w = worker.lock();
                w.thread_ctx = thread_ctx;
                w.status = MailboxWorkerStatus::Running {
                    dispatch_id: dispatch_id.clone(),
                    run_id: run_id.clone(),
                    lease_handle,
                    sink: Arc::clone(&reconnectable_sink),
                };
            }

            // Spawn execution.
            self.spawn_execution(
                claimed_dispatch,
                reconnectable_sink,
                claim_token,
                thread_id.clone(),
                suspended,
            );

            Ok((
                MailboxSubmitResult {
                    dispatch_id,
                    run_id,
                    thread_id,
                    status: MailboxDispatchStatus::Running,
                },
                event_rx,
            ))
        } else {
            // Inline claim failed (another claimed dispatch exists for this
            // thread). Cancel the orphaned dispatch to prevent it from
            // lingering with the guard available_at.
            let now_fix = now_ms();
            let cancel_start = Instant::now();
            let cancel_result = self.store.cancel(&dispatch_id, now_fix).await;
            record_mailbox_operation_result("cancel", result_label(&cancel_result), cancel_start);
            match cancel_result {
                Ok(Some(cancelled_dispatch)) => {
                    self.mark_cancelled_dispatch_run_cancelled(
                        &cancelled_dispatch,
                        "inline dispatch cancelled after claim race",
                    )
                    .await;
                    self.refresh_dispatch_depth_metrics().await;
                }
                Ok(None) => {
                    if let Ok(Some(dispatch)) = self.store.load_dispatch(&dispatch_id).await {
                        self.reconcile_terminal_dispatch(&dispatch).await;
                    }
                    self.refresh_dispatch_depth_metrics().await;
                }
                Err(e) => {
                    tracing::warn!(dispatch_id, error = %e, "failed to cancel unclaimed inline dispatch");
                }
            }
            Err(MailboxError::Validation(ACTIVE_RUN_CONFLICT_MESSAGE.into()))
        }
    }

    /// Submit a run in the background (fire-and-forget).
    ///
    /// Dispatch is persisted with `available_at = now`, then execution is event-driven.
    /// Returns dispatch_id + thread_id for status polling.
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id()))]
    pub async fn submit_background(
        self: &Arc<Self>,
        mut request: RunActivation,
    ) -> Result<MailboxSubmitResult, MailboxError> {
        normalize_mailbox_run_mode(&mut request, true);
        let (thread_id, messages) = validate_run_inputs(
            request.thread_id().to_owned(),
            request.messages().to_vec(),
            !request.control.seeded_decisions.is_empty(),
        )?;

        self.ensure_dispatch_id_hint(&mut request);
        let run_id = self
            .prepare_run_for_dispatch(&mut request, &thread_id, &messages)
            .await?;
        let dispatch = self.build_dispatch(&request, &thread_id)?;
        let dispatch_id = dispatch.dispatch_id().clone();
        let thread_id = dispatch.thread_id().clone();

        // WAL: persist with available_at = now; startup recovery reconstructs
        // the row if the process crashed after preparing the run checkpoint.
        self.enqueue_dispatch_for_request(&request, &dispatch)
            .await?;
        self.record_mailbox_dispatch_event("RunQueued", &dispatch)
            .await;

        // Dispatch via try_dispatch_next which handles Idle → Claiming atomically.
        self.get_or_create_worker(&thread_id).await;
        let claimed = self.try_dispatch_next(&thread_id).await;
        let status = if claimed.started_execution() {
            MailboxDispatchStatus::Running
        } else {
            MailboxDispatchStatus::Queued
        };

        Ok(MailboxSubmitResult {
            dispatch_id,
            run_id,
            thread_id,
            status,
        })
    }

    /// Try to steer the currently active run first, then fall back to the
    /// durable mailbox queue when live delivery is unavailable.
    ///
    /// Delivery remains at-least-once: a live ack can be lost after `try_send`
    /// succeeds, forcing a durable fallback with the same message. Callers
    /// that need exactly-once effects must provide application idempotency.
    #[tracing::instrument(skip(self, request), fields(thread_id = %request.thread_id()))]
    pub async fn submit_live_then_queue(
        self: &Arc<Self>,
        mut request: RunActivation,
        expected_run_id: Option<&str>,
    ) -> Result<MailboxSubmitResult, MailboxError> {
        let (thread_id, messages) = validate_run_inputs(
            request.thread_id().to_owned(),
            request.messages().to_vec(),
            !request.control.seeded_decisions.is_empty(),
        )?;
        let messages = normalize_message_ids(&messages);

        if let Some(result) = self
            .try_deliver_live_messages(&thread_id, expected_run_id, messages.clone())
            .await?
        {
            return Ok(result);
        }

        request.intent.thread_id = thread_id;
        request.input = RunInput::NewMessages(messages);
        self.submit_background(request).await
    }

    // ── Run preparation & reconstruction ─────────────────────────────

    fn ensure_dispatch_id_hint(&self, request: &mut RunActivation) -> String {
        match request.persistence.dispatch_id_hint.as_ref() {
            Some(id) if !id.trim().is_empty() => id.clone(),
            _ => {
                let id = uuid::Uuid::now_v7().to_string();
                request.persistence.dispatch_id_hint = Some(id.clone());
                id
            }
        }
    }

    /// Create or update the durable run truth before enqueuing a dispatch.
    ///
    /// The caller assigns `dispatch_id_hint` before this method persists the
    /// checkpoint. Startup recovery can then reconcile the crash window where
    /// the run checkpoint landed but the matching dispatch WAL write did not.
    pub(super) async fn prepare_run_for_dispatch(
        &self,
        request: &mut RunActivation,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<String, MailboxError> {
        let _append_guard = lock_thread_append(&self.thread_append_locks, thread_id).await;
        if request.resume_run_id().is_none()
            && request.persistence.run_id_hint.is_none()
            && let Some(waiting_run_id) = self.reusable_waiting_run_id(thread_id).await?
        {
            request.intent.kind = RunKind::HitlResume {
                run_id: waiting_run_id,
            };
            request.trace.run_mode = RunMode::Resume;
        }

        let run_id = request
            .resume_run_id()
            .map(str::to_owned)
            .or_else(|| request.persistence.run_id_hint.clone())
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        if request.resume_run_id().is_none() {
            request.persistence.run_id_hint = Some(run_id.clone());
        }
        let dispatch_id = self.ensure_dispatch_id_hint(request);

        let normalized_messages = normalize_message_ids(messages);
        let input_message_ids = normalized_messages
            .iter()
            .filter_map(|message| message.id.clone())
            .collect::<Vec<_>>();
        let previous_run = self.run_store.latest_run(thread_id).await?;

        // Build the message-count-independent run record template and pinned
        // manifest once. The existing-run vs new-run decision is stable across
        // append retries: a version-conflicting append never commits the run,
        // so `load_run(run_id)` is invariant under retry.
        let existing_run = self.run_store.load_run(&run_id).await?;
        let (mut record, resolution_id) = if let Some(mut existing) = existing_run {
            if existing.thread_id != thread_id {
                return Err(MailboxError::Validation(format!(
                    "run_id '{run_id}' belongs to thread '{}', not '{thread_id}'",
                    existing.thread_id
                )));
            }
            if existing.status != RunStatus::Created && !existing.is_resumable_waiting() {
                return Err(MailboxError::Validation(format!(
                    "run_id '{run_id}' is not open for dispatch"
                )));
            }
            if request.intent.agent_id.is_none() {
                request.intent.agent_id = Some(existing.agent_id.clone());
            }
            let resolution_id = if let Some(id) = existing.resolution_id.clone() {
                id
            } else {
                let id = self
                    .resolve_replayable_resolution_id(
                        request,
                        request.persistence.resolution_id_hint.clone(),
                        run_id.clone(),
                    )
                    .await?;
                existing.resolution_id = Some(id.clone());
                id
            };
            existing.request = None;
            existing.dispatch_id = Some(dispatch_id);
            (existing, resolution_id)
        } else {
            let inferred_agent_id = request
                .intent
                .agent_id
                .clone()
                .or_else(|| {
                    previous_run.as_ref().and_then(|run| {
                        (run.status != RunStatus::Created && !run.agent_id.trim().is_empty())
                            .then(|| run.agent_id.clone())
                    })
                })
                .unwrap_or_else(|| "default".to_string());
            let inherited_state = previous_run
                .as_ref()
                .filter(|run| run.status != RunStatus::Created)
                .and_then(|run| run.state.clone());
            if request.intent.agent_id.is_none() {
                request.intent.agent_id = Some(inferred_agent_id.clone());
            }
            let resolution_id = self
                .resolve_replayable_resolution_id(
                    request,
                    request.persistence.resolution_id_hint.clone(),
                    run_id.clone(),
                )
                .await?;
            let now = now_ms() / 1000;
            let record = RunRecord {
                run_id: run_id.clone(),
                thread_id: thread_id.to_string(),
                agent_id: inferred_agent_id,
                parent_run_id: request.trace.parent_run_id.clone(),
                resolution_id: Some(resolution_id.clone()),
                activation: None,
                request: None,
                input: None,
                output: None,
                status: RunStatus::Created,
                termination_reason: None,
                final_output: None,
                error_payload: None,
                dispatch_id: Some(dispatch_id),
                session_id: None,
                transport_request_id: request.trace.transport_request_id.clone(),
                waiting: None,
                outcome: None,
                created_at: now,
                started_at: None,
                finished_at: None,
                updated_at: now,
                steps: 0,
                input_tokens: 0,
                output_tokens: 0,
                state: inherited_state,
            };
            (record, resolution_id)
        };

        if let Some(run_id) = self
            .prepare_pending_messages_for_dispatch(
                request,
                thread_id,
                &normalized_messages,
                &run_id,
                &mut record,
                resolution_id.as_str(),
            )
            .await?
        {
            return Ok(run_id);
        }

        // Append-only committed write with reload-merge retry (ADR-0042 A).
        const MAX_APPEND_ATTEMPTS: usize = 8;
        for _ in 0..MAX_APPEND_ATTEMPTS {
            let existing_messages = self
                .run_store
                .load_committed_messages(thread_id)
                .await?
                .unwrap_or_default();
            let expected_version = existing_messages.len() as u64;
            let first_new_seq = expected_version + 1;
            let last_new_seq = expected_version + normalized_messages.len() as u64;
            let (input_snapshot, input) =
                build_run_input(thread_id, last_new_seq, &input_message_ids);
            record.activation = Some(run_activation_snapshot(
                request,
                input_snapshot,
                Some(resolution_id.clone()),
            ));
            record.input = input;
            record.updated_at = now_ms() / 1000;

            if self
                .commit_run_append(
                    thread_id,
                    &normalized_messages,
                    Some(expected_version),
                    &record,
                )
                .await?
            {
                // The CAS guarantees nothing was appended since the read, so the
                // committed log is exactly `existing_messages ++ delta`.
                let mut appended_messages = existing_messages;
                appended_messages.extend(normalized_messages.iter().cloned());
                // This eager-append path treats the server canonical events as
                // advisory (ADR-0042 D7: dispatch/append may rely on
                // recovery-safe compensation, unlike `freeze`). A publish
                // failure here is logged inside record_*_events and must not
                // block the live terminal event for this run; the missing events
                // are reconciled by repair_thread_message_checkpoint_events.
                if let Err(error) = self
                    .record_thread_message_checkpoint_events(
                        thread_id,
                        &run_id,
                        &appended_messages,
                        first_new_seq,
                        last_new_seq,
                    )
                    .await
                {
                    tracing::warn!(
                        thread_id,
                        run_id,
                        error = %error,
                        "advisory checkpoint event publish failed on eager append; will be reconciled by repair"
                    );
                }
                self.refresh_worker_checkpoint_cache(thread_id, &appended_messages, &record)
                    .await;
                return Ok(run_id);
            }
        }

        Err(MailboxError::Internal(format!(
            "committed append exhausted {MAX_APPEND_ATTEMPTS} retries under version conflict for thread '{thread_id}'"
        )))
    }

    /// Validate that `request` resolves to a replayable plan bound to
    /// `resolution_id`, returning the id the plan carries. The server owns the
    /// resolution id; the runtime treats it as opaque.
    async fn resolve_replayable_resolution_id(
        &self,
        request: &RunActivation,
        resolution_id_hint: Option<String>,
        fallback_resolution_id: String,
    ) -> Result<String, MailboxError> {
        let resolution_scope = match resolution_id_hint {
            Some(resolution_id) => RegistryResolutionScope::Pinned(resolution_id),
            None if request.inherited.run_resolver.is_some() => RegistryResolutionScope::Live,
            None => RegistryResolutionScope::Pinned(fallback_resolution_id),
        };
        self.executor
            .resolve_activation_in_scope(
                request,
                ResolutionPolicy::PersistentServer,
                resolution_scope,
            )
            .await
            .and_then(|plan| plan.into_replayable())
            .map(|plan| plan.artifact.resolution_id)
            .map_err(|source| MailboxError::Resolution {
                context: "resolving persistent mailbox dispatch to a replayable run",
                source,
            })
    }

    /// Build a RunDispatch from the durable run prepared above.
    pub(super) fn build_dispatch(
        &self,
        request: &RunActivation,
        thread_id: &str,
    ) -> Result<RunDispatch, MailboxError> {
        let run_id = request
            .resume_run_id()
            .map(str::to_owned)
            .or_else(|| request.persistence.run_id_hint.clone())
            .ok_or_else(|| MailboxError::Internal("run_id missing after preparation".into()))?;
        let now = now_ms();
        Ok(RunDispatch::queued(
            request
                .persistence
                .dispatch_id_hint
                .clone()
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
            thread_id.to_string(),
            run_id,
            now,
        )
        .with_max_attempts(self.config.default_max_attempts))
    }

    pub(super) async fn reconstruct_run_request(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<RunActivation, MailboxError> {
        let run = {
            let cached = {
                let workers = self.workers.read().await;
                workers.get(dispatch.thread_id()).and_then(|w| {
                    let w = w.lock();
                    w.thread_ctx
                        .as_ref()
                        .and_then(|ctx| ctx.get_run(&dispatch.run_id()).cloned())
                })
            };
            if let Some(run) = cached {
                run
            } else {
                self.run_store
                    .load_run(&dispatch.run_id())
                    .await?
                    .ok_or_else(|| {
                        MailboxError::Validation(format!(
                            "run '{}' not found for dispatch '{}'",
                            dispatch.run_id(),
                            dispatch.dispatch_id()
                        ))
                    })?
            }
        };
        if run.thread_id != *dispatch.thread_id() {
            return Err(MailboxError::Validation(format!(
                "run '{}' belongs to thread '{}', not dispatch thread '{}'",
                run.run_id,
                run.thread_id,
                dispatch.thread_id()
            )));
        }
        if let Some(snapshot) = run.activation.clone() {
            let activation_messages = self
                .activation_messages_for_snapshot(&run, &snapshot.input)
                .await?;
            let mut request = RunActivation::new(run.thread_id.clone(), activation_messages)
                .with_messages_already_persisted(true);
            request.intent = snapshot.intent;
            request.options = snapshot.options;
            request.trace = snapshot.trace;
            request.control.seeded_decisions = snapshot.seeded_decisions;
            return self
                .attach_dispatch_replay_wiring(&run, dispatch, request)
                .await;
        }

        let snapshot = run.request.clone().ok_or_else(|| {
            MailboxError::Validation(format!("run '{}' has no activation snapshot", run.run_id))
        })?;
        let activation_messages = self.activation_messages_for_run(&run, &snapshot).await?;
        let extras = snapshot
            .request_extras
            .as_ref()
            .map(|extras_value| {
                LegacyRunSnapshotExtras::from_value(extras_value).map_err(|error| {
                    MailboxError::Validation(format!("corrupt request_extras: {error}"))
                })
            })
            .transpose()?;
        let resolution_id = run.resolution_id.clone();
        let input = legacy_input_snapshot(&run, &snapshot);
        let snapshot = awaken_server_contract::contract::run::RunActivationSnapshot::try_from(
            LegacyRunRequestSnapshotAdapter {
                snapshot,
                input,
                resolution_id,
                thread_id: run.thread_id.clone(),
                agent_id: (!run.agent_id.trim().is_empty()).then(|| run.agent_id.clone()),
                parent_run_id: run.parent_run_id.clone(),
                extras,
            },
        )
        .map_err(|error| {
            MailboxError::Validation(format!("legacy run snapshot conversion failed: {error}"))
        })?;
        let mut request = RunActivation::new(run.thread_id.clone(), activation_messages)
            .with_messages_already_persisted(true);
        request.intent = snapshot.intent;
        request.options = snapshot.options;
        request.trace = snapshot.trace;
        request.control.seeded_decisions = snapshot.seeded_decisions;
        self.attach_dispatch_replay_wiring(&run, dispatch, request)
            .await
    }

    async fn attach_dispatch_replay_wiring(
        &self,
        run: &RunRecord,
        dispatch: &RunDispatch,
        mut request: RunActivation,
    ) -> Result<RunActivation, MailboxError> {
        request = if run.is_resumable_waiting() {
            request.intent.kind = RunKind::HitlResume {
                run_id: run.run_id.clone(),
            };
            if request.trace.run_mode == RunMode::Foreground {
                request.trace.run_mode = RunMode::Resume;
            }
            request
        } else {
            request.with_run_id_hint(run.run_id.clone())
        };
        Ok(request.with_trace_dispatch_id(dispatch.dispatch_id().clone()))
    }

    async fn activation_messages_for_snapshot(
        &self,
        run: &RunRecord,
        snapshot: &RunInputSnapshot,
    ) -> Result<Vec<Message>, MailboxError> {
        if snapshot.trigger_message_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.activation_messages_by_ids(run, &snapshot.trigger_message_ids)
            .await
    }

    async fn activation_messages_for_run(
        &self,
        run: &RunRecord,
        snapshot: &RunRequestSnapshot,
    ) -> Result<Vec<Message>, MailboxError> {
        if snapshot.input_message_ids.is_empty() {
            return self.activation_messages_from_range(run, snapshot).await;
        }
        self.activation_messages_by_ids(run, &snapshot.input_message_ids)
            .await
    }

    async fn activation_messages_by_ids(
        &self,
        run: &RunRecord,
        message_ids: &[String],
    ) -> Result<Vec<Message>, MailboxError> {
        let cached_messages: Option<Vec<Message>> = {
            let workers = self.workers.read().await;
            workers.get(&run.thread_id).and_then(|w| {
                let w = w.lock();
                w.thread_ctx.as_ref().and_then(|ctx| {
                    let mut msgs = Vec::with_capacity(message_ids.len());
                    for msg_id in message_ids {
                        let found = ctx
                            .messages
                            .iter()
                            .find(|m| m.id.as_deref() == Some(msg_id.as_str()));
                        msgs.push(found?.clone());
                    }
                    Some(msgs)
                })
            })
        };
        if let Some(msgs) = cached_messages {
            return Ok(msgs);
        }
        let mut messages = Vec::with_capacity(message_ids.len());
        for message_id in message_ids {
            let record = self
                .run_store
                .load_message_record(&run.thread_id, message_id)
                .await?
                .ok_or_else(|| {
                    MailboxError::Validation(format!(
                        "message '{message_id}' not found for run '{}'",
                        run.run_id
                    ))
                })?;
            messages.push(record.message);
        }
        Ok(messages)
    }

    async fn activation_messages_from_range(
        &self,
        run: &RunRecord,
        snapshot: &RunRequestSnapshot,
    ) -> Result<Vec<Message>, MailboxError> {
        let Some(input) = run.input.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(range) = input.range else {
            return Ok(Vec::new());
        };
        let count = snapshot.input_message_count;
        if count == 0 {
            return Ok(Vec::new());
        }
        let from_seq = range.to_seq.saturating_sub(count).saturating_add(1);
        let Some(range) = MessageSeqRange::new(from_seq.max(range.from_seq), range.to_seq) else {
            return Ok(Vec::new());
        };
        let records = self
            .run_store
            .load_message_records_range(&run.thread_id, range)
            .await?;
        Ok(records.into_iter().map(|record| record.message).collect())
    }
}
