use super::core::apply_context_messages_to_prompt;
use super::state_commit::PendingDeltaCommitContext;
use super::stream_core::preallocate_tool_result_message_ids;
use super::*;
use crate::runtime::streaming::StreamRecoveryCheckpoint;
use std::collections::HashSet;

// Stream adapter layer:
// - drives provider I/O and plugin phases
// - emits AgentEvent stream
// - delegates deterministic state-machine helpers to `stream_core`

#[derive(Debug)]
struct StreamEventEmitter {
    run_id: String,
    thread_id: String,
    parent_run_id: Option<String>,
    seq: u64,
    step_index: u32,
    current_step_id: Option<String>,
}

impl StreamEventEmitter {
    fn new(run_id: String, thread_id: String, parent_run_id: Option<String>) -> Self {
        Self {
            run_id,
            thread_id,
            parent_run_id,
            seq: 0,
            step_index: 0,
            current_step_id: None,
        }
    }

    fn run_start(&mut self) -> AgentEvent {
        self.emit(AgentEvent::RunStart {
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            parent_run_id: self.parent_run_id.clone(),
        })
    }

    fn run_finish(&mut self, outcome: LoopOutcome) -> AgentEvent {
        self.emit(outcome.to_run_finish_event(self.run_id.clone()))
    }

    fn step_start(&mut self, message_id: String) -> AgentEvent {
        self.current_step_id = Some(format!("step:{}", self.step_index));
        self.emit(AgentEvent::StepStart { message_id })
    }

    fn step_end(&mut self) -> AgentEvent {
        let event = self.emit(AgentEvent::StepEnd);
        self.step_index = self.step_index.saturating_add(1);
        self.current_step_id = None;
        event
    }

    fn emit_existing(&mut self, event: AgentEvent) -> AgentEvent {
        self.emit(event)
    }

    fn emit(&mut self, event: AgentEvent) -> AgentEvent {
        let seq = self.seq;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        super::event_envelope_meta::register_runtime_event_envelope_meta(
            &event,
            &self.run_id,
            &self.thread_id,
            seq,
            timestamp_ms,
            self.current_step_id.clone(),
        );
        self.seq = self.seq.saturating_add(1);
        tracing::trace!(
            run_id = %self.run_id,
            thread_id = %self.thread_id,
            parent_run_id = %self.parent_run_id.clone().unwrap_or_default(),
            seq,
            timestamp_ms,
            step_id = %self.current_step_id.clone().unwrap_or_default(),
            event_type = %event_type_name(&event),
            "emit agent event"
        );
        event
    }
}

fn event_type_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::RunStart { .. } => "run_start",
        AgentEvent::RunFinish { .. } => "run_finish",
        AgentEvent::TextDelta { .. } => "text_delta",
        AgentEvent::ReasoningDelta { .. } => "reasoning_delta",
        AgentEvent::ReasoningEncryptedValue { .. } => "reasoning_encrypted_value",
        AgentEvent::ToolCallStart { .. } => "tool_call_start",
        AgentEvent::ToolCallDelta { .. } => "tool_call_delta",
        AgentEvent::ToolCallReady { .. } => "tool_call_ready",
        AgentEvent::ToolCallDone { .. } => "tool_call_done",
        AgentEvent::StepStart { .. } => "step_start",
        AgentEvent::StepEnd => "step_end",
        AgentEvent::InferenceComplete { .. } => "inference_complete",
        AgentEvent::StateSnapshot { .. } => "state_snapshot",
        AgentEvent::StateDelta { .. } => "state_delta",
        AgentEvent::MessagesSnapshot { .. } => "messages_snapshot",
        AgentEvent::ActivitySnapshot { .. } => "activity_snapshot",
        AgentEvent::ActivityDelta { .. } => "activity_delta",
        AgentEvent::ToolCallResumed { .. } => "tool_call_resumed",
        AgentEvent::Error { .. } => "error",
    }
}

fn genai_usage_has_tokens(usage: &genai::chat::Usage) -> bool {
    usage.prompt_tokens.is_some()
        || usage.completion_tokens.is_some()
        || usage.total_tokens.is_some()
        || usage
            .prompt_tokens_details
            .as_ref()
            .is_some_and(|details| !details.is_empty())
        || usage
            .completion_tokens_details
            .as_ref()
            .is_some_and(|details| !details.is_empty())
}

fn stream_result_has_usage(result: &StreamResult) -> bool {
    result.usage.as_ref().is_some_and(|usage| {
        usage.prompt_tokens.is_some()
            || usage.completion_tokens.is_some()
            || usage.total_tokens.is_some()
            || usage.cache_read_tokens.is_some()
            || usage.cache_creation_tokens.is_some()
            || usage.thinking_tokens.is_some()
    })
}

fn stream_event_has_payload(event: &genai::chat::ChatStreamEvent) -> bool {
    match event {
        genai::chat::ChatStreamEvent::Start => false,
        genai::chat::ChatStreamEvent::Chunk(chunk)
        | genai::chat::ChatStreamEvent::ReasoningChunk(chunk)
        | genai::chat::ChatStreamEvent::ThoughtSignatureChunk(chunk) => !chunk.content.is_empty(),
        genai::chat::ChatStreamEvent::ToolCallChunk(tool_chunk) => {
            let tool_call = &tool_chunk.tool_call;
            !tool_call.call_id.is_empty()
                || !tool_call.fn_name.is_empty()
                || !matches!(tool_call.fn_arguments, serde_json::Value::Null)
        }
        genai::chat::ChatStreamEvent::End(end) => {
            end.captured_usage
                .as_ref()
                .is_some_and(genai_usage_has_tokens)
                || end.captured_stop_reason.is_some()
                || end
                    .captured_reasoning_content
                    .as_ref()
                    .is_some_and(|value| !value.is_empty())
                || end
                    .captured_content
                    .as_ref()
                    .is_some_and(|content| !content.is_empty())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingEventKey {
    kind: &'static str,
    id: String,
}

fn pending_event_key(event: &AgentEvent) -> Option<PendingEventKey> {
    match event {
        AgentEvent::ToolCallStart { id, .. } => Some(PendingEventKey {
            kind: "tool_start",
            id: id.clone(),
        }),
        AgentEvent::ToolCallReady { id, .. } => Some(PendingEventKey {
            kind: "tool_ready",
            id: id.clone(),
        }),
        _ => None,
    }
}

pub(super) fn run_stream(
    agent: Arc<dyn Agent>,
    tools: HashMap<String, Arc<dyn Tool>>,
    run_ctx: RunContext,
    run_identity: RunIdentity,
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
    Box::pin(stream! {
    let mut run_ctx = run_ctx;
    let mut decision_rx = decision_rx;
    let mut pending_decisions = std::collections::VecDeque::new();
    let executor = llm_executor_for_run(agent.as_ref());
    let mut run_state = LoopRunState::new();
    let mut last_text = String::new();
    let mut continued_response_prefix = String::new();
    let mut stream_retry_model_preference: Option<String> = None;
    let mut stream_retry_backoff_window = RetryBackoffWindow::default();
    let mut stream_error_counts_by_model: HashMap<String, usize> = HashMap::new();
    let run_cancellation_token = cancellation_token;
    let step_tool_provider = step_tool_provider_for_run(agent.as_ref(), tools);
        let (activity_tx, mut activity_rx) = tokio::sync::mpsc::unbounded_channel();
        let activity_manager: Arc<dyn ActivityManager> = Arc::new(ActivityHub::new(activity_tx));

        let run_id = run_identity.run_id.clone();
        let parent_run_id = run_identity.parent_run_id.clone();
        let baseline_suspended_call_ids = suspended_call_ids(&run_ctx);
        let pending_delta_commit = PendingDeltaCommitContext::new(
            &run_identity,
            state_committer.as_ref(),
        );
        let mut emitter =
            StreamEventEmitter::new(run_id.clone(), run_ctx.thread_id().to_string(), parent_run_id.clone());
        let mut active_tool_snapshot = match resolve_step_tool_snapshot(&step_tool_provider, &run_ctx).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                let message = e.to_string();
                yield emitter.emit_existing(AgentEvent::Error {
                    message: message.clone(),
                    code: Some("STATE_ERROR".to_string()),
                });
                let outcome = build_loop_outcome(
                    run_ctx,
                    TerminationReason::Error(message.clone()),
                    None,
                    &run_state,
                    Some(outcome::LoopFailure::State(message)),
                );
                yield emitter.run_finish(outcome);
                return;
            }
        };
        let mut active_tool_descriptors = active_tool_snapshot.descriptors.clone();

        macro_rules! terminate_stream_error {
            ($failure:expr, $message:expr) => {{
                let failure = $failure;
                let message = $message;
                let code = match &failure {
                    outcome::LoopFailure::Llm(_) => Some("LLM_ERROR".to_string()),
                    outcome::LoopFailure::State(_) => Some("STATE_ERROR".to_string()),
                };
                if let Err(e) = persist_run_termination(
                    &mut run_ctx,
                    &TerminationReason::Error(message.clone()),
                    &active_tool_descriptors,
                    agent.as_ref(),
                    &run_identity,
                    &pending_delta_commit,
                    run_state.token_totals(),
                )
                .await
                {
                    yield emitter.emit_existing(AgentEvent::Error {
                        message: e.to_string(),
                        code: Some("STATE_ERROR".to_string()),
                    });
                    return;
                }
                let outcome = build_loop_outcome(
                    run_ctx,
                    TerminationReason::Error(message.clone()),
                    Some(last_text.clone()),
                    &run_state,
                    Some(failure),
                );
                yield emitter.emit_existing(AgentEvent::Error {
                    message: message.clone(),
                    code,
                });
                yield emitter.run_finish(outcome);
                return;
            }};
        }

        macro_rules! finish_run {
            ($termination_expr:expr, $response_expr:expr) => {{
                let reason: TerminationReason = $termination_expr;
                let (final_termination, final_response) = normalize_termination_for_suspended_calls(
                    &run_ctx,
                    reason,
                    $response_expr,
                );
                if let Err(e) = persist_run_termination(
                    &mut run_ctx,
                    &final_termination,
                    &active_tool_descriptors,
                    agent.as_ref(),
                    &run_identity,
                    &pending_delta_commit,
                    run_state.token_totals(),
                )
                .await
                {
                    yield emitter.emit_existing(AgentEvent::Error {
                        message: e.to_string(),
                        code: Some("STATE_ERROR".to_string()),
                    });
                    return;
                }
                let outcome = build_loop_outcome(
                    run_ctx,
                    final_termination,
                    final_response,
                    &run_state,
                    None,
                );
                yield emitter.run_finish(outcome);
                return;
            }};
        }

        // Phase: RunStart (use scoped block to manage borrow)
        match plugin_runtime::emit_phase_block(
            Phase::RunStart,
            &run_ctx,
            &active_tool_descriptors,
            agent.as_ref(),
            |_| {},
        )
            .await
        {
            Ok((pending, actions)) => {
                run_ctx.add_thread_patches(pending);
                run_ctx.add_serialized_state_actions(actions);
            }
            Err(e) => {
                let message = e.to_string();
                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
            }
        }

        yield emitter.run_start();

        let run_start_drain = match commit_run_start_and_drain_replay(
            &mut run_ctx,
            &active_tool_snapshot.tools,
            agent.as_ref(),
            &active_tool_descriptors,
            &pending_delta_commit,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let message = e.to_string();
                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
            }
        };

        let mut emitted_run_start_pending_keys = HashSet::new();
        for event in run_start_drain.events {
            if let Some(key) = pending_event_key(&event) {
                emitted_run_start_pending_keys.insert(key);
            }
            yield emitter.emit_existing(event);
        }

        let run_start_new_suspended =
            newly_suspended_call_ids(&run_ctx, &baseline_suspended_call_ids);
        if !run_start_new_suspended.is_empty() {
            for event in suspended_call_pending_events_for_ids(&run_ctx, &run_start_new_suspended) {
                let should_emit = match pending_event_key(&event) {
                    Some(key) => emitted_run_start_pending_keys.insert(key),
                    None => true,
                };
                if should_emit {
                    yield emitter.emit_existing(event);
                }
            }
            finish_run!(TerminationReason::Suspended, None);
        }

        let mut context_tracker = ContextThrottleTracker::new();
        let mut step_counter: usize = 0;

        'step: loop {
            let decision_events = match apply_decisions_and_replay(
                &mut run_ctx,
                &mut decision_rx,
                &mut pending_decisions,
                &step_tool_provider,
                agent.as_ref(),
                &mut active_tool_descriptors,
                &pending_delta_commit,
            )
            .await
            {
                Ok(events) => events,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };
            if !decision_events.is_empty() {
                for event in decision_events {
                    yield emitter.emit_existing(event);
                }
            }

            // Check cancellation at the top of each iteration.
            if is_run_cancelled(run_cancellation_token.as_ref()) {
                finish_run!(TerminationReason::Cancelled, None);
            }

            active_tool_snapshot = match resolve_step_tool_snapshot(&step_tool_provider, &run_ctx).await {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };
            active_tool_descriptors = active_tool_snapshot.descriptors.clone();

            let prepared = match prepare_step_execution(&run_ctx, &active_tool_descriptors, agent.as_ref()).await {
                Ok(v) => v,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };
            run_ctx.add_thread_patches(prepared.pending_patches);
            run_ctx.add_serialized_state_actions(prepared.serialized_state_actions);
            let mut messages = prepared.messages;
            let filtered_tools = prepared.filtered_tools;
            let request_transforms = prepared.request_transforms;
            let step_inference_override = prepared.inference_override;

            apply_context_messages_to_prompt(
                &mut messages,
                &mut context_tracker,
                prepared.context_messages,
                step_counter,
                !agent.system_prompt().is_empty(),
            );
            step_counter = step_counter.saturating_add(1);

            match prepared.run_action {
                RunAction::Continue => {}
                RunAction::Terminate(reason) => {
                    if matches!(reason, TerminationReason::Suspended) {
                        for event in suspended_call_pending_events(&run_ctx) {
                            yield emitter.emit_existing(event);
                        }
                    }
                    let response = if matches!(reason, TerminationReason::BehaviorRequested) {
                        Some(last_text.clone())
                    } else {
                        None
                    };
                    finish_run!(reason, response);
                }
            }

            // Step boundary: starting LLM call
            let assistant_msg_id = gen_message_id();
            yield emitter.step_start(assistant_msg_id.clone());

            // Stream LLM response with unified retry + fallback model strategy.
            let chat_options = apply_inference_override(
                agent.chat_options(),
                step_inference_override.as_ref(),
            );
            let attempt_outcome = run_llm_with_retry_and_fallback(
                agent.as_ref(),
                run_cancellation_token.as_ref(),
                agent.llm_retry_policy().retry_stream_start,
                stream_retry_model_preference.as_deref(),
                step_inference_override.as_ref(),
                "unknown llm stream start error",
                |model| {
                    let request =
                        build_request_for_filtered_tools(&messages, &active_tool_snapshot.tools, &filtered_tools, &request_transforms);
                    let executor = executor.clone();
                    let chat_options = chat_options.clone();
                    async move {
                        executor
                            .exec_chat_stream_events(&model, request, chat_options.as_ref())
                            .await
                    }
                },
            )
            .await;

            let (chat_stream_events, inference_model) = match attempt_outcome {
                LlmAttemptOutcome::Success {
                    value,
                    model,
                    attempts,
                } => {
                    run_state.record_llm_attempts(attempts);
                    (value, model)
                }
                LlmAttemptOutcome::Cancelled => {
                    append_cancellation_user_message(&mut run_ctx, CancellationStage::Inference);
                    finish_run!(TerminationReason::Cancelled, None);
                }
                LlmAttemptOutcome::Exhausted {
                    last_error,
                    last_error_class,
                    attempts,
                } => {
                    run_state.record_llm_attempts(attempts);
                        match apply_llm_error_cleanup(
                            &mut run_ctx,
                            &active_tool_descriptors,
                            agent.as_ref(),
                            "llm_stream_start_error",
                            last_error.clone(),
                            last_error_class,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(phase_error) => {
                            let message = phase_error.to_string();
                            terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                        }
                    }
                    let message = last_error;
                    terminate_stream_error!(outcome::LoopFailure::Llm(message.clone()), message);
                }
            };

            // Collect streaming response
            let inference_start = std::time::Instant::now();
            let mut collector = StreamCollector::new();
            let mut chat_stream = chat_stream_events;
            let mut saw_stream_payload = false;

            loop {
                let next_event = if let Some(ref token) = run_cancellation_token {
                    tokio::select! {
                        _ = token.cancelled() => {
                            append_cancellation_user_message(&mut run_ctx, CancellationStage::Inference);
                            finish_run!(TerminationReason::Cancelled, None);
                        }
                        decision = recv_decision(&mut decision_rx), if decision_rx.is_some() => {
                            let Some(response) = decision else {
                                decision_rx = None;
                                continue;
                            };
                            let decision_outcome = match apply_decision_and_replay(
                                &mut run_ctx,
                                response,
                                &mut decision_rx,
                                &mut pending_decisions,
                                DecisionReplayInputs {
                                    step_tool_provider: &step_tool_provider,
                                    agent: agent.as_ref(),
                                    active_tool_descriptors: &mut active_tool_descriptors,
                                },
                                &pending_delta_commit,
                            )
                            .await
                            {
                                Ok(outcome) => outcome,
                                Err(e) => {
                                    let message = e.to_string();
                                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                                }
                            };
                            for event in decision_outcome.events {
                                yield emitter.emit_existing(event);
                            }
                            continue;
                        }
                        ev = chat_stream.next() => ev,
                    }
                } else if decision_rx.is_some() {
                    tokio::select! {
                        decision = recv_decision(&mut decision_rx), if decision_rx.is_some() => {
                            let Some(response) = decision else {
                                decision_rx = None;
                                continue;
                            };
                            let decision_outcome = match apply_decision_and_replay(
                                &mut run_ctx,
                                response,
                                &mut decision_rx,
                                &mut pending_decisions,
                                DecisionReplayInputs {
                                    step_tool_provider: &step_tool_provider,
                                    agent: agent.as_ref(),
                                    active_tool_descriptors: &mut active_tool_descriptors,
                                },
                                &pending_delta_commit,
                            )
                            .await
                            {
                                Ok(outcome) => outcome,
                                Err(e) => {
                                    let message = e.to_string();
                                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                                }
                            };
                            for event in decision_outcome.events {
                                yield emitter.emit_existing(event);
                            }
                            continue;
                        }
                        ev = chat_stream.next() => ev,
                    }
                } else {
                    chat_stream.next().await
                };

                let Some(event_result) = next_event else {
                    break;
                };

                match event_result {
                    Ok(event) => {
                        if stream_event_has_payload(&event) {
                            saw_stream_payload = true;
                        }
                        if let Some(output) = collector.process(event) {
                            match output {
                                crate::runtime::streaming::StreamOutput::TextDelta(delta) => {
                                    yield emitter.emit_existing(AgentEvent::TextDelta { delta });
                                }
                                crate::runtime::streaming::StreamOutput::ReasoningDelta(delta) => {
                                    yield emitter.emit_existing(AgentEvent::ReasoningDelta { delta });
                                }
                                crate::runtime::streaming::StreamOutput::ReasoningEncryptedValue(encrypted_value) => {
                                    yield emitter.emit_existing(AgentEvent::ReasoningEncryptedValue { encrypted_value });
                                }
                                crate::runtime::streaming::StreamOutput::ToolCallStart { id, name } => {
                                    yield emitter.emit_existing(AgentEvent::ToolCallStart { id, name });
                                }
                                crate::runtime::streaming::StreamOutput::ToolCallDelta { id, args_delta } => {
                                    yield emitter.emit_existing(AgentEvent::ToolCallDelta { id, args_delta });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let error_message = e.to_string();
                        let error_class = classify_llm_error(&e);
                        let max_stream_retries = agent.llm_retry_policy().max_stream_event_retries;
                        if error_class.is_retryable()
                            && truncation_recovery::should_retry_stream_error(
                                &mut run_state,
                                max_stream_retries,
                            )
                        {
                            let failure_count = stream_error_counts_by_model
                                .entry(inference_model.clone())
                                .or_insert(0);
                            *failure_count += 1;
                            let fallback_threshold = agent
                                .llm_retry_policy()
                                .stream_error_fallback_threshold
                                .max(1);
                            let escalate_to_fallback = *failure_count >= fallback_threshold;
                            stream_retry_model_preference = if escalate_to_fallback {
                                next_llm_model_after(agent.as_ref(), &inference_model)
                                    .or_else(|| Some(inference_model.clone()))
                            } else {
                                Some(inference_model.clone())
                            };
                            let recovery_checkpoint = collector.into_recovery_checkpoint();
                            tracing::warn!(
                                error = %error_message,
                                class = ?error_class,
                                retry = run_state.stream_event_retries,
                                recovery = ?recovery_checkpoint,
                                next_model = %stream_retry_model_preference.clone().unwrap_or_else(|| inference_model.clone()),
                                "mid-stream error, recovering stream"
                            );
                            match wait_retry_backoff(
                                agent.as_ref(),
                                run_state.stream_event_retries,
                                &mut stream_retry_backoff_window,
                                run_cancellation_token.as_ref(),
                            )
                            .await
                            {
                                RetryBackoffOutcome::Completed => {}
                                RetryBackoffOutcome::Cancelled => {
                                    append_cancellation_user_message(
                                        &mut run_ctx,
                                        CancellationStage::Inference,
                                    );
                                    finish_run!(TerminationReason::Cancelled, None);
                                }
                                RetryBackoffOutcome::BudgetExhausted => {
                                    tracing::warn!(
                                        error = %error_message,
                                        retry = run_state.stream_event_retries,
                                        "mid-stream retry budget exhausted"
                                    );
                                    match apply_llm_error_cleanup(
                                        &mut run_ctx,
                                        &active_tool_descriptors,
                                        agent.as_ref(),
                                        "llm_stream_event_error",
                                        error_message.clone(),
                                        Some(error_class.as_str()),
                                    )
                                    .await
                                    {
                                        Ok(()) => {}
                                        Err(phase_error) => {
                                            let message = phase_error.to_string();
                                            terminate_stream_error!(
                                                outcome::LoopFailure::State(message.clone()),
                                                message
                                            );
                                        }
                                    }
                                    terminate_stream_error!(
                                        outcome::LoopFailure::Llm(error_message.clone()),
                                        error_message
                                    );
                                }
                            }
                            match recovery_checkpoint {
                                StreamRecoveryCheckpoint::NoPayload => {}
                                StreamRecoveryCheckpoint::PartialText(partial_text) => {
                                    let msg = assistant_message(&partial_text);
                                    run_ctx.add_message(Arc::new(msg));
                                    extend_response_prefix(
                                        &mut continued_response_prefix,
                                        &partial_text,
                                    );
                                    last_text = continued_response_prefix.clone();
                                    let continuation =
                                        truncation_recovery::stream_error_continuation_message();
                                    run_ctx.add_message(Arc::new(continuation));
                                }
                                StreamRecoveryCheckpoint::ToolCallObserved => {
                                    continued_response_prefix.clear();
                                }
                            }
                            // Close the failed step and re-enter the outer loop.
                            yield emitter.step_end();
                            mark_step_completed(&mut run_state);
                            continue 'step;
                        }
                        // Non-retryable or retries exhausted: terminate the run.
                        match apply_llm_error_cleanup(
                            &mut run_ctx,
                            &active_tool_descriptors,
                            agent.as_ref(),
                            "llm_stream_event_error",
                            error_message.clone(),
                            Some(error_class.as_str()),
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(phase_error) => {
                                let message = phase_error.to_string();
                                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                            }
                        }
                        terminate_stream_error!(outcome::LoopFailure::Llm(error_message.clone()), error_message);
                    }
                }
            }

            let max_output_tokens = chat_options.as_ref().and_then(|o| o.max_tokens);
            let result = collector.finish(max_output_tokens);

            // Empty stream response: the stream completed without error but
            // produced no content, tool calls, or usage.  This is almost always
            // a provider-side transient fault (e.g. an SSE error payload that
            // genai silently discarded).  Treat it like a retryable mid-stream
            // error so the run is not terminated prematurely.
            if !saw_stream_payload
                && result.text.is_empty()
                && result.tool_calls.is_empty()
                && !stream_result_has_usage(&result)
            {
                let max_stream_retries = agent.llm_retry_policy().max_stream_event_retries;
                if truncation_recovery::should_retry_stream_error(
                    &mut run_state,
                    max_stream_retries,
                ) {
                    let error_message = format!(
                        "empty stream response from model='{inference_model}' (no content, tool calls, or usage); retrying"
                    );
                    tracing::warn!(
                        error = %error_message,
                        retry = run_state.stream_event_retries,
                        "empty stream response, recovering"
                    );
                    match wait_retry_backoff(
                        agent.as_ref(),
                        run_state.stream_event_retries,
                        &mut stream_retry_backoff_window,
                        run_cancellation_token.as_ref(),
                    )
                    .await
                    {
                        RetryBackoffOutcome::Completed => {}
                        RetryBackoffOutcome::Cancelled => {
                            append_cancellation_user_message(
                                &mut run_ctx,
                                CancellationStage::Inference,
                            );
                            finish_run!(TerminationReason::Cancelled, None);
                        }
                        RetryBackoffOutcome::BudgetExhausted => {
                            tracing::warn!(
                                error = %error_message,
                                retry = run_state.stream_event_retries,
                                "empty stream retry budget exhausted"
                            );
                            match apply_llm_error_cleanup(
                                &mut run_ctx,
                                &active_tool_descriptors,
                                agent.as_ref(),
                                "llm_stream_event_error",
                                error_message.clone(),
                                None,
                            )
                            .await
                            {
                                Ok(()) => {}
                                Err(phase_error) => {
                                    let message = phase_error.to_string();
                                    terminate_stream_error!(
                                        outcome::LoopFailure::State(message.clone()),
                                        message
                                    );
                                }
                            }
                            terminate_stream_error!(
                                outcome::LoopFailure::Llm(error_message.clone()),
                                error_message
                            );
                        }
                    }
                    yield emitter.step_end();
                    mark_step_completed(&mut run_state);
                    continue 'step;
                }

                // Retries exhausted — terminate.
                let error_message = format!(
                    "empty stream response from model='{inference_model}' (no content, tool calls, or usage); possible upstream SSE error payload was ignored"
                );
                match apply_llm_error_cleanup(
                    &mut run_ctx,
                    &active_tool_descriptors,
                    agent.as_ref(),
                    "llm_stream_event_error",
                    error_message.clone(),
                    None,
                )
                .await
                {
                    Ok(()) => {}
                    Err(phase_error) => {
                        let message = phase_error.to_string();
                        terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                    }
                }
                terminate_stream_error!(outcome::LoopFailure::Llm(error_message.clone()), error_message);
            }

            // Successful stream — reset retry state.
            stream_retry_model_preference = None;
            stream_retry_backoff_window.reset();
            stream_error_counts_by_model.clear();
            last_text = stitch_response_text(&continued_response_prefix, &result.text);
            run_state.update_from_response(&result);
            let inference_duration_ms = inference_start.elapsed().as_millis() as u64;

            yield emitter.emit_existing(AgentEvent::InferenceComplete {
                model: inference_model,
                usage: result.usage.clone(),
                duration_ms: inference_duration_ms,
            });

            let step_meta = step_metadata(Some(run_id.clone()), run_state.completed_steps as u32);
            let post_inference_action = match complete_step_after_inference(
                &mut run_ctx,
                &result,
                step_meta.clone(),
                Some(assistant_msg_id.clone()),
                &active_tool_descriptors,
                agent.as_ref(),
            )
            .await
            {
                Ok(action) => action,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };

            if let Err(e) = pending_delta_commit
                .commit(&mut run_ctx, CheckpointReason::AssistantTurnCommitted, false)
                .await
            {
                let message = e.to_string();
                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
            }

            // Step boundary: finished LLM call
            yield emitter.step_end();

            mark_step_completed(&mut run_state);

            // Truncation recovery: if the model hit max_tokens with no tool
            // calls, inject a continuation prompt and re-enter inference.
            if truncation_recovery::should_retry(&result, &mut run_state) {
                extend_response_prefix(&mut continued_response_prefix, &result.text);
                let prompt = truncation_recovery::continuation_message();
                run_ctx.add_message(std::sync::Arc::new(prompt));
                continue;
            }
            continued_response_prefix.clear();

            // Extract termination reason for deferred handling.
            let post_inference_termination = match &post_inference_action {
                RunAction::Terminate(reason) => Some(reason.clone()),
                _ => None,
            };

            // Only `Stopped` termination is deferred past tool execution so
            // the current round's tools complete (e.g. MaxRounds lets tools
            // finish).  All other reasons terminate immediately.
            if let Some(reason) = &post_inference_termination {
                if !matches!(reason, TerminationReason::Stopped(_)) {
                    if matches!(reason, TerminationReason::Suspended) {
                        for event in suspended_call_pending_events(&run_ctx) {
                            yield emitter.emit_existing(event);
                        }
                    }
                    finish_run!(reason.clone(), Some(last_text.clone()));
                }
            }

            // Check if we need to execute tools
            if !result.needs_tools() {
                run_state.record_step_without_tools();
                if is_run_cancelled(run_cancellation_token.as_ref()) {
                    finish_run!(TerminationReason::Cancelled, None);
                }
                let reason = post_inference_termination.unwrap_or(TerminationReason::NaturalEnd);
                finish_run!(reason, Some(last_text.clone()));
            }

            // Emit ToolCallReady for each finalized tool call
            for tc in &result.tool_calls {
                yield emitter.emit_existing(AgentEvent::ToolCallReady {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                });
            }

            // Execute tools with phase hooks
            let tool_context = match prepare_tool_execution_context(&run_ctx) {
                Ok(ctx) => ctx,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };
            let sid_for_tools = run_ctx.thread_id().to_string();
            let thread_messages_for_tools = run_ctx.messages().to_vec();
            let thread_version_for_tools = run_ctx.version();
            let tool_descriptors_for_exec = active_tool_descriptors.clone();
            let mut tool_future: Pin<
                Box<dyn Future<Output = Result<Vec<ToolExecutionResult>, AgentLoopError>> + Send>,
            > = Box::pin(async {
                agent
                    .tool_executor()
                    .execute(ToolExecutionRequest {
                        tools: &active_tool_snapshot.tools,
                        calls: &result.tool_calls,
                        state: &tool_context.state,
                        tool_descriptors: &tool_descriptors_for_exec,
                        agent_behavior: Some(agent.behavior()),
                        activity_manager: activity_manager.clone(),
                        run_policy: &tool_context.run_policy,
                        run_identity: tool_context.run_identity.clone(),
                        caller_context: tool_context.caller_context.clone(),
                        thread_id: &sid_for_tools,
                        thread_messages: &thread_messages_for_tools,
                        state_version: thread_version_for_tools,
                        cancellation_token: run_cancellation_token.as_ref(),
                    })
                    .await
                    .map_err(AgentLoopError::from)
            });
            let mut activity_closed = false;
            let mut resolved_call_ids = HashSet::new();
            let results = loop {
                tokio::select! {
                    activity = activity_rx.recv(), if !activity_closed => {
                        match activity {
                            Some(event) => {
                                yield emitter.emit_existing(event);
                            }
                            None => {
                                activity_closed = true;
                            }
                        }
                    }
                    decision = recv_decision(&mut decision_rx), if decision_rx.is_some() => {
                        let Some(response) = decision else {
                            decision_rx = None;
                            continue;
                        };
                        let decision_outcome = match apply_decision_and_replay(
                            &mut run_ctx,
                            response,
                            &mut decision_rx,
                            &mut pending_decisions,
                            DecisionReplayInputs {
                                step_tool_provider: &step_tool_provider,
                                agent: agent.as_ref(),
                                active_tool_descriptors: &mut active_tool_descriptors,
                            },
                            &pending_delta_commit,
                        )
                        .await
                        {
                            Ok(outcome) => outcome,
                            Err(e) => {
                                let message = e.to_string();
                                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                            }
                        };
                        for call_id in decision_outcome.resolved_call_ids {
                            resolved_call_ids.insert(call_id);
                        }
                        for event in decision_outcome.events {
                            yield emitter.emit_existing(event);
                        }
                    }
                    res = &mut tool_future => {
                        break res;
                    }
                }
            };

            while let Ok(event) = activity_rx.try_recv() {
                yield emitter.emit_existing(event);
            }

            let mut results = match results {
                Ok(r) => r,
                Err(AgentLoopError::Cancelled) => {
                    append_cancellation_user_message(&mut run_ctx, CancellationStage::ToolExecution);
                    finish_run!(TerminationReason::Cancelled, None);
                }
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };
            if !resolved_call_ids.is_empty() {
                results.retain(|exec_result| {
                    !(matches!(
                        exec_result.outcome,
                        crate::contracts::ToolCallOutcome::Suspended
                    )
                        && resolved_call_ids.contains(&exec_result.execution.call.id))
                });
            }

            // Emit suspended-call events first.
            for exec_result in &results {
                if let Some(ref suspended_call) = exec_result.suspended_call {
                    // If pending projection reuses the original call id, start/ready
                    // was already streamed from model output.
                    if suspended_call.ticket.pending.id == suspended_call.call_id {
                        continue;
                    }
                    for event in pending_tool_events(suspended_call) {
                        yield emitter.emit_existing(event);
                    }
                }
            }
            // Pre-generate message IDs for tool results so streaming events
            // and stored Messages share the same ID.
            let tool_msg_ids = preallocate_tool_result_message_ids(&results);

            let applied = match apply_tool_results_impl(
                &mut run_ctx,
                &results,
                Some(step_meta),
                agent.tool_executor().requires_parallel_patch_conflict_check(),
                Some(&tool_msg_ids),
            ) {
                Ok(a) => a,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };

            if let Err(e) = pending_delta_commit
                .commit(&mut run_ctx, CheckpointReason::ToolResultsCommitted, false)
                .await
            {
                let message = e.to_string();
                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
            }

            let decision_events = match apply_decisions_and_replay(
                &mut run_ctx,
                &mut decision_rx,
                &mut pending_decisions,
                &step_tool_provider,
                agent.as_ref(),
                &mut active_tool_descriptors,
                &pending_delta_commit,
            )
            .await
            {
                Ok(events) => events,
                Err(e) => {
                    let message = e.to_string();
                    terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                }
            };
            for event in decision_events {
                yield emitter.emit_existing(event);
            }

            // Emit non-pending tool results (pending ones pause the run).
            for exec_result in &results {
                if !matches!(
                    exec_result.outcome,
                    crate::contracts::ToolCallOutcome::Suspended
                ) {
                    yield emitter.emit_existing(AgentEvent::ToolCallDone {
                        id: exec_result.execution.call.id.clone(),
                        result: exec_result.execution.result.clone(),
                        patch: exec_result.execution.patch.clone(),
                        message_id: tool_msg_ids.get(&exec_result.execution.call.id).cloned().unwrap_or_default(),
                    });
                }
            }

            // Emit state snapshot when we mutated state (tool patches or pending/clear).
            if let Some(snapshot) = applied.state_snapshot {
                yield emitter.emit_existing(AgentEvent::StateSnapshot { snapshot });
            }

            // If ALL tools are suspended (no completed results), terminate immediately.
            if has_suspended_calls(&run_ctx) {
                let has_completed = results.iter().any(|r| {
                    !matches!(r.outcome, crate::contracts::ToolCallOutcome::Suspended)
                });
                if !has_completed {
                    finish_run!(TerminationReason::Suspended, None);
                }
            }

            // Deferred post-inference termination: tools from the current round
            // have completed; stop the loop before the next inference.
            if let Some(reason) = post_inference_termination {
                finish_run!(reason, Some(last_text.clone()));
            }

            // Track tool step metrics for stop condition evaluation.
            let error_count = results
                .iter()
                .filter(|r| r.execution.result.is_error())
                .count();
            run_state.record_tool_step(&result.tool_calls, error_count);

        }
    })
}

#[cfg(test)]
mod tests {
    use super::stream_event_has_payload;
    use genai::chat::{ChatStreamEvent, StreamEnd};

    #[test]
    fn stream_end_with_captured_stop_reason_counts_as_payload() {
        let event = ChatStreamEvent::End(StreamEnd {
            captured_stop_reason: Some(genai::chat::StopReason::from("stop".to_string())),
            ..Default::default()
        });
        assert!(stream_event_has_payload(&event));
    }

    #[test]
    fn stream_end_without_any_captured_values_has_no_payload() {
        let event = ChatStreamEvent::End(StreamEnd::default());
        assert!(!stream_event_has_payload(&event));
    }
}
