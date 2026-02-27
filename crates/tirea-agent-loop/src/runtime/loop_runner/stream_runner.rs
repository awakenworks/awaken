use super::state_commit::PendingDeltaCommitContext;
use super::stream_core::{preallocate_tool_result_message_ids, resolve_stream_run_identity};
use super::*;
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
    cancellation_token: Option<RunCancellationToken>,
    state_committer: Option<Arc<dyn StateCommitter>>,
    decision_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ToolCallDecision>>,
) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
    Box::pin(stream! {
    let mut run_ctx = run_ctx;
    let mut decision_rx = decision_rx;
    let mut pending_decisions = std::collections::VecDeque::new();
    let executor = llm_executor_for_run(agent.as_ref());
    let mut run_state = RunState::new();
    let mut last_text = String::new();
    let run_cancellation_token = cancellation_token;
    let step_tool_provider = step_tool_provider_for_run(agent.as_ref(), tools);
        let (activity_tx, mut activity_rx) = tokio::sync::mpsc::unbounded_channel();
        let activity_manager: Arc<dyn ActivityManager> = Arc::new(ActivityHub::new(activity_tx));

        let run_identity = resolve_stream_run_identity(&mut run_ctx);
        let run_id = run_identity.run_id;
        let parent_run_id = run_identity.parent_run_id;
        let baseline_suspended_call_ids = suspended_call_ids(&run_ctx);
        let pending_delta_commit = PendingDeltaCommitContext::new(
            &run_id,
            parent_run_id.as_deref(),
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
                });
                let outcome = build_loop_outcome(
                    run_ctx,
                    TerminationReason::Error,
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
                if let Err(e) = persist_run_termination(
                    &mut run_ctx,
                    &TerminationReason::Error,
                    &active_tool_descriptors,
                    agent.as_ref(),
                    &pending_delta_commit,
                    RunFinishedCommitPolicy::BestEffort,
                )
                .await
                {
                    yield emitter.emit_existing(AgentEvent::Error {
                        message: e.to_string(),
                    });
                    return;
                }
                let outcome = build_loop_outcome(
                    run_ctx,
                    TerminationReason::Error,
                    Some(last_text.clone()),
                    &run_state,
                    Some(failure),
                );
                yield emitter.emit_existing(AgentEvent::Error {
                    message: message.clone(),
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
                    &pending_delta_commit,
                    RunFinishedCommitPolicy::Required,
                )
                .await
                {
                    yield emitter.emit_existing(AgentEvent::Error {
                        message: e.to_string(),
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
            Ok(pending) => {
                run_ctx.add_thread_patches(pending);
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

        loop {
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
            let messages = prepared.messages;
            let filtered_tools = prepared.filtered_tools;

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
            let chat_options = agent.chat_options().cloned();
            let attempt_outcome = run_llm_with_retry_and_fallback(
                agent.as_ref(),
                run_cancellation_token.as_ref(),
                agent.llm_retry_policy().retry_stream_start,
                "unknown llm stream start error",
                |model| {
                    let request =
                        build_request_for_filtered_tools(&messages, &active_tool_snapshot.tools, &filtered_tools);
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
                    attempts,
                } => {
                    run_state.record_llm_attempts(attempts);
                        match apply_llm_error_cleanup(
                            &mut run_ctx,
                            &active_tool_descriptors,
                            agent.as_ref(),
                            "llm_stream_start_error",
                            last_error.clone(),
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
                                &step_tool_provider,
                                agent.as_ref(),
                                &mut active_tool_descriptors,
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
                                &step_tool_provider,
                                agent.as_ref(),
                                &mut active_tool_descriptors,
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
                        match apply_llm_error_cleanup(
                            &mut run_ctx,
                            &active_tool_descriptors,
                            agent.as_ref(),
                            "llm_stream_event_error",
                            e.to_string(),
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(phase_error) => {
                                let message = phase_error.to_string();
                                terminate_stream_error!(outcome::LoopFailure::State(message.clone()), message);
                            }
                        }
                        let message = e.to_string();
                        terminate_stream_error!(outcome::LoopFailure::Llm(message.clone()), message);
                    }
                }
            }

            let result = collector.finish();
            last_text = result.text.clone();
            run_state.update_from_response(&result);
            let inference_duration_ms = inference_start.elapsed().as_millis() as u64;

            yield emitter.emit_existing(AgentEvent::InferenceComplete {
                model: inference_model,
                usage: result.usage.clone(),
                duration_ms: inference_duration_ms,
            });

            let step_meta = step_metadata(Some(run_id.clone()), run_state.completed_steps as u32);
            let post_inference_termination = match complete_step_after_inference(
                &mut run_ctx,
                &result,
                step_meta.clone(),
                Some(assistant_msg_id.clone()),
                &active_tool_descriptors,
                agent.as_ref(),
            )
            .await
            {
                Ok(reason) => reason,
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
                        run_config: &tool_context.run_config,
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
                            &step_tool_provider,
                            agent.as_ref(),
                            &mut active_tool_descriptors,
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
