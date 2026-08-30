//! The Agent step loop and its per-step orchestration.

use super::*;

pub(crate) async fn run_agent_loop(
    runtime: &Runtime,
    activation: RunActivation,
    context: RuntimeRunContext,
) -> Result<RunState> {
    let mut resolved = runtime
        .resolve(&activation.snapshot)
        .map_err(map_resolver_error)?;
    if let Some(model_ref) = activation
        .model_ref_override
        .as_deref()
        .filter(|model_ref| !model_ref.is_empty())
        && !resolved.spec.select_execution_model(model_ref)
    {
        return Err(Error::Execution(format!(
            "model override `{model_ref}` is outside the publication-pinned candidate set"
        )));
    }

    let run_id = activation.run_id.clone();
    let thread_id = activation.thread_id.clone();
    let delegation_origin = activation.delegation_origin.clone();

    // Admission owns the input before any model-side capability is realized.
    // A cancellation or fail-closed plugin resolve still commits that accepted
    // input atomically with the terminal state, so recovery can prove the exact
    // request that produced the outcome instead of masking it as missing history.
    let run_input: std::sync::Arc<[Message]> = activation.input.clone().into();
    let (committed, fresh_input) =
        committed_history_and_fresh_input(&context, &thread_id, activation.input);
    let mut transcript = model_transcript(
        &context,
        committed,
        &run_id,
        &thread_id,
        &activation.snapshot.fingerprint.0,
        &activation.snapshot.resolved_spec.instructions,
    );

    // Cancellation observed before any model call: commit a terminal Cancelled
    // outcome instead of starting work.
    if context.is_cancelled() {
        let step =
            RunStepResult::ended_with_messages(run_id.clone(), EndCause::Cancelled, fresh_input);
        return finish(runtime, &context, &thread_id, run_id, step).await;
    }

    // Merge the active plugins under their capability bounds; a violation fails
    // the run closed before any model call (G30).
    let env = match runtime.resolve_plugin_env_with(&resolved.spec, &context.session_plugins) {
        Ok(env) => env,
        Err(error) => {
            tracing::warn!(
                run_id = %run_id.0,
                thread_id = %thread_id.0,
                error = %error,
                "run plugin environment violates its declared capability boundary"
            );
            let step = RunStepResult::ended_with_messages(
                run_id.clone(),
                EndCause::Error(Failure::CapabilityBound),
                fresh_input,
            );
            return finish(runtime, &context, &thread_id, run_id, step).await;
        }
    };

    emit(&context, &run_id, AgentEvent::Fact(Fact::RunStarted)).await;

    // A fresh run continues the thread's conversation: seed the transcript with
    // the committed history (when a reader is wired), then this run's input —
    // but only the input NOT already in that history. A reclaimed run re-executes
    // from its activation, yet its input was committed as the first step delta by
    // the prior attempt, so it is already present: appending it again would both
    // show the model the input twice AND re-commit it (the durable accumulator's
    // watermark resets per attempt, so it cannot tell). Keyed on the stable
    // message id, a fresh run's uncommitted input passes through unchanged while a
    // reclaimed or redelivered input is dropped — input delivery is idempotent.
    transcript.extend(fresh_input.iter().cloned());
    let store = store_from_commands(
        context
            .reader
            .as_ref()
            .map(|reader| reader.committed_state(&thread_id))
            .unwrap_or_default(),
        &run_id,
    );
    let step_result = drive(
        runtime,
        &resolved,
        &env,
        &run_id,
        &thread_id,
        &context,
        delegation_origin.as_ref(),
        transcript,
        fresh_input,
        run_input,
        0,
        store,
        Vec::new(),
        Vec::new(),
    )
    .await?;
    finalize(runtime, &context, &thread_id, run_id, step_result).await
}

/// Tracks the active plugins' live tool-face version and lazily re-resolves the
/// execution environment when it changes (e.g. an MCP server firing
/// `tools/list_changed`). A static run keeps `last_version` fixed and never
/// re-resolves, so it is zero-overhead; a failed re-resolution keeps the prior
/// environment rather than aborting the run.
struct LiveEnv {
    resolved: Option<ResolvedExecutionEnv>,
    last_version: Option<u64>,
}

impl LiveEnv {
    fn new(initial_version: Option<u64>) -> Self {
        Self {
            resolved: None,
            last_version: initial_version,
        }
    }

    /// The environment to use this step: the freshly re-resolved one if the live
    /// version advanced (best-effort), else `base`.
    fn current<'a>(
        &'a mut self,
        runtime: &Runtime,
        resolved: &ResolvedRun,
        session_plugins: &[std::sync::Arc<dyn awaken_runtime_contract::plugin::Plugin>],
        base: &'a ResolvedExecutionEnv,
    ) -> &'a ResolvedExecutionEnv {
        let version = runtime.active_live_version_with(&resolved.spec.plugin_ids, session_plugins);
        if version != self.last_version {
            if let Ok(refreshed) = runtime.resolve_plugin_env_with(&resolved.spec, session_plugins)
            {
                self.resolved = Some(refreshed);
            }
            self.last_version = version;
        }
        self.resolved.as_ref().unwrap_or(base)
    }
}

/// Run one step's inference to a final response, applying two in-place recoveries
/// before yielding. A `MaxTokens`-truncated text-only step is continued in place:
/// the partial is committed as its own assistant message, a continuation prompt
/// follows it, and inference reruns on the grown transcript — up to a per-step
/// budget, after which the outer loop commits the last partial and ends with an
/// explicit incomplete-output inference failure. A truncated step still carrying
/// tool calls skips recovery: those calls must be answered by tool results, not
/// another assistant step. On a clean pre-commit failure (no partial committed
/// this step) with a candidate remaining, it fails over to the next pool model;
/// the breaker inside `infer_with_retry` keys on `request.model_binding`, so a
/// failed-over model tracks its own circuit and usage. A failure after a committed
/// partial is terminal here — mid-stream recovery is the StreamCheckpoint/resume
/// path, not a switch to a different model (which would double-generate).
#[allow(clippy::too_many_arguments)]
async fn infer_step(
    llm: &std::sync::Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    step_base: usize,
    step: usize,
    ledger: &mut StepLedger,
    store: &mut Store,
    prelude: &[Message],
    discovery: &ToolDiscoveryState,
    checkpoint_ref: Option<&CheckpointCtx<'_>>,
    step_resume: Option<StreamCheckpoint>,
    context: &RuntimeRunContext,
    context_window: Option<usize>,
    final_step: bool,
) -> std::result::Result<InferStepOutcome, awaken_runtime_contract::llm::Error> {
    let mut truncation_retries = 0;
    // Model-pool failover: the ordered candidate bindings (primary first, then
    // any pool fallbacks). Single-model agents yield exactly one, so this is a
    // no-op for them. `cand_idx` advances only on a clean pre-commit failure.
    let candidates = resolved.spec.candidate_bindings();
    let mut cand_idx = 0usize;
    let mut pending_resume = step_resume;
    loop {
        let mut request = build_chat_request_checked(
            &resolved.spec,
            prelude,
            &ledger.transcript,
            &env.dynamic_descriptors(),
            discovery,
            !final_step
                && context.tool_capability_narrowing
                    == awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured,
        )?;
        if final_step {
            // `max_steps` is a runaway bound, but a run that spends its last
            // allowance on another tool call cannot report the evidence it has
            // already gathered. Reserve the last inference for a natural final
            // response: hide every tool and make the boundary explicit without
            // mutating the durable transcript.
            request.messages.push(ChatMessage {
                role: Role::System,
                content: vec![ContentBlock::text(
                    "This is the final inference step. Do not request or invoke tools. Return the best complete natural final response now, obey the original output contract, and explicitly report every incomplete check or blocker.",
                )],
            });
        }
        if let Some(keep_last) = context_window {
            // Compaction supplied complete prefix coverage in `prelude` (summary
            // plus any bridge). Protect that request-only prefix and window only
            // the parent transcript tail.
            let protected = usize::from(!resolved.spec.instructions.is_empty()) + prelude.len();
            let transcript = request.messages.split_off(protected);
            let transcript =
                apply_context_policy(&ContextPolicy::KeepLast { keep_last }, transcript);
            request.messages.extend(transcript);
        }
        request.model_binding = candidates[cand_idx].clone();
        if let Some(materializer) = &context.model_content_materializer {
            request = materializer.materialize(request).await?;
        }
        if let Some(gate) = &context.model_request_gate {
            match gate
                .admit_model_request(ModelRequestAdmissionRequest {
                    run_id: run_id.clone(),
                    thread_id: thread_id.clone(),
                })
                .await
                .map_err(|error| {
                    awaken_runtime_contract::llm::Error::Unauthorized(format!(
                        "model request admission unavailable: {error}"
                    ))
                })? {
                ModelRequestAdmission::Admit => {}
                ModelRequestAdmission::Pause(reason) => {
                    break Ok(InferStepOutcome::Await(reason));
                }
            }
        }
        // One stable response coordinate per model response. A MaxTokens
        // continuation constructs a fresh sink on the next loop iteration, so
        // its preview cannot be merged into the partial Message that just
        // committed.
        let delta_sink = StreamDeltaSink {
            context,
            run_id,
            thread_id,
            step: step_base + step,
            response: truncation_retries,
        };
        let observed = infer_with_retry_observed(
            llm,
            request,
            runtime.retry_policy(),
            runtime.circuit_breaker(),
            &delta_sink,
            checkpoint_ref,
            pending_resume.take(),
            &context.capture.decision,
            context.content_sink(),
            runtime.metrics(),
            context.ownership.as_deref(),
        )
        .await;
        // The audit vector shares the existing ThreadCommit watermarks with
        // messages and state. Gate pauses return before this seam and therefore
        // stage no model observation.
        ledger
            .audit
            .push(RunEvent::ModelRequestCompleted(observed.observation).into());
        match observed.result {
            Ok(response) => {
                let has_tool_calls = !response.output.tool_calls().is_empty();
                let has_text = !response.output.text_content().is_empty();
                let has_continuable_output = has_text || response.output.has_reasoning();
                let may_continue = response.stop_reason.is_some_and(|reason| {
                    reason.admits_continuation(
                        has_tool_calls,
                        has_continuable_output,
                        truncation_retries,
                        runtime.max_continuation_retries(),
                    )
                });
                if may_continue {
                    if let Some(step_usage) = response.usage {
                        fold_thread_usage(
                            store,
                            &mut ledger.staged_state,
                            "thread usage state drifted; skipping continuation record",
                            |usage| usage.record(&candidates[cand_idx].model_ref, step_usage),
                        );
                    }
                    let partial = truncated_assistant_message(
                        run_id,
                        step_base + step,
                        truncation_retries,
                        response.output.blocks,
                    );
                    ledger.push_message(partial);
                    let prompt = continuation_message(run_id, step_base + step, truncation_retries);
                    ledger.push_message(prompt);
                    // The next loop iteration is another logical model request.
                    // Commit this response's usage and transcript first so the
                    // owning Session gate observes the exact cumulative truth.
                    if context.commit.is_some() && ledger.has_uncommitted() {
                        ledger
                            .commit_delta(context, thread_id, run_id)
                            .await
                            .map_err(|error| {
                                awaken_runtime_contract::llm::Error::Provider(error.to_string())
                            })?;
                    }
                    truncation_retries += 1;
                    continue;
                }
                break Ok(InferStepOutcome::Response {
                    response,
                    model_ref: candidates[cand_idx].model_ref.clone(),
                });
            }
            Err(err) => {
                if truncation_retries == 0 && cand_idx + 1 < candidates.len() {
                    cand_idx += 1;
                    // A resume checkpoint belongs to the prior model's stream;
                    // a fresh candidate starts clean.
                    pending_resume = None;
                    continue;
                }
                break Err(err);
            }
        }
    }
}

enum InferStepOutcome {
    Response {
        response: ChatResponse,
        model_ref: String,
    },
    Await(PauseReason),
}

/// Run the model/tool loop over a prepared transcript. Shared by fresh execution
/// and resume; the caller seeds the transcript, the already-produced messages,
/// and any state the resume itself staged (`seed_state`).
// The agent-invocation span in OTel GenAI terms: this is the reasoning loop that
// drives one agent run to completion, so it carries `gen_ai.operation.name =
// "invoke_agent"` and parents the `chat` / `execute_tool` spans. `SpanKind::Internal`;
// `gen_ai.provider.name` is the neutral `awaken` (G22). Awaken run/thread ids ride
// alongside as `awaken.*` extensions.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "invoke_agent",
    skip_all,
    fields(
        otel.kind = "internal",
        gen_ai.operation.name = "invoke_agent",
        gen_ai.provider.name = "awaken",
        gen_ai.conversation.id = %thread_id.0,
        awaken.run.id = %run_id.0,
        awaken.thread.id = %thread_id.0,
    )
)]
pub(super) async fn drive(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    context: &RuntimeRunContext,
    delegation_origin: Option<&DelegationOrigin>,
    transcript: Vec<Message>,
    new_messages: Vec<Message>,
    run_input: std::sync::Arc<[Message]>,
    step_base: usize,
    mut store: Store,
    seed_state: Vec<StateCommand>,
    seed_audit: Vec<EventDraft>,
) -> Result<RunStepResult> {
    // The per-run model executor (ADR-0004) wins over the runtime's bound default:
    // the host resolves the run's model ref to an executor for this attempt and
    // injects it here, so a database-less worker runs the run's own configured model
    // without a local provider credential. The runtime only uses the executor — it
    // never learns how the model is reached. Absent → the runtime's session-resolved
    // executor.
    let llm = context
        .model_executor
        .clone()
        .or_else(|| runtime.llm().cloned())
        .ok_or_else(|| {
            Error::Execution("no model provider configured for this runtime".to_string())
        })?;

    // The attempt's durable-progress accumulator; it owns the step-commit
    // watermark invariant, so only the tail beyond a watermark is ever returned
    // or re-committed.
    let mut ledger = StepLedger::new(transcript, new_messages, seed_state, seed_audit);
    // The loop's terminal decision. It stays `None` only if the loop runs to its
    // step ceiling, which is itself an end (`MaxSteps`).
    let mut disposition: Option<RunDisposition> = None;
    // Durable interrupted-stream checkpoints (Phase 3), when a store is wired.
    // A checkpoint left by a crash mid-recovery is read once here and applied to
    // this drive's first step — the only step that can be resuming, since
    // per-step commit means at most one step was ever in flight.
    let checkpoint_ctx = context
        .stream_checkpoint
        .as_ref()
        .map(|store| CheckpointCtx {
            store: store.as_ref(),
            run_id: run_id.0.clone(),
            thread_id: thread_id.0.clone(),
            model: resolved.spec.model_binding.model_ref.clone(),
        });
    let mut resume_checkpoint = match &checkpoint_ctx {
        Some(ctx) => match ctx.store.get(&ctx.run_id).await {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                tracing::warn!(run_id = %ctx.run_id, %error, "failed to load stream checkpoint");
                None
            }
        },
        None => None,
    };

    // How many times a run-end guard has steered this run. It is both the
    // guard's iteration signal and the runtime's run-scoped continuation count;
    // `max_steps` remains the hard runaway backstop, since each steer costs a step.
    // Recovered from the committed transcript by counting this run's own steer
    // feedback messages, so the count (and thus the guard's budget) survives a
    // await/resume mid-loop rather than restarting at zero.
    // Recover the run's forced-continuation count from committed truth: the steer
    // messages already in the transcript. The id type classifies its own kind, so
    // the loop reads a fact rather than matching an id-string convention by hand.
    let mut forced_continuations = ledger
        .transcript
        .iter()
        .filter(|message| message.id.is_steer_of(run_id))
        .count();

    // The agent's configured ceiling guards against a non-terminating tool cycle;
    // a natural-end text step ends the loop earlier. A plugin whose tool set is
    // dynamic (e.g. an MCP server firing `tools/list_changed`) advances its
    // `live_version`; `live_env` re-resolves the environment at the step boundary
    // when it changes, and is zero-overhead for static runs.
    let mut live_env = LiveEnv::new(
        runtime.active_live_version_with(&resolved.spec.plugin_ids, &context.session_plugins),
    );
    // Failed inference steps in a row (post-retry). The runtime's tolerance
    // decides when the streak is terminal; a success resets it.
    let mut consecutive_inference_failures = 0usize;
    // Deferred tools (ADR-0053) the model has discovered via `tool_search` this run, by
    // canonical id: once revealed, a tool's full schema is sent on subsequent steps.
    let mut discovery =
        ToolDiscoveryStateKey::load(&store).map_err(|error| Error::Execution(error.to_string()))?;

    // A reclaimed Running run may have died after committing a Requested/Executing
    // tool batch. Recover that explicit Run-scoped entity before asking the model
    // for another Step. Terminal calls are reused, never re-entered.
    if let Some(batch) = ActiveToolBatch::load(&store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .filter(|batch| batch.run_id() == run_id && batch.phase() != ToolBatchPhase::Finalized)
    {
        match dispatch::recover_tool_batch(
            runtime,
            context,
            delegation_origin,
            resolved,
            env,
            run_id,
            thread_id,
            batch,
            &mut ledger,
            &mut store,
            &mut discovery,
        )
        .await
        {
            Ok(Some(reached)) => return Ok(ledger.into_step_result(reached)),
            Ok(None) => {}
            Err(Error::StateConflict) => {
                return Ok(ledger.into_step_result(RunDisposition::ended(
                    run_id.clone(),
                    EndCause::Error(Failure::StateConflict),
                )));
            }
            Err(error) => return Err(error),
        }
    }

    for step in 0..resolved.spec.max_steps {
        // Step-boundary incremental commit: everything the completed steps
        // staged — messages, state, audit — becomes durable under a `Running`
        // fact before the next inference. A crash then loses at most the step
        // in flight, and readers see committed progress mid-run. The staged
        // batch is validated cumulatively (the tail alone could hide an
        // exclusive-key conflict with an already-committed step); a conflict
        // ends the run as `finalize` would, except steps committed while the
        // batch was still valid stay committed. Without a coordinator the
        // block is inert and the whole batch rides `finalize`, as before.
        if context.commit.is_some() && ledger.has_uncommitted() {
            if validate_batch(&ledger.staged_state).is_err() {
                ledger.rollback_state();
                disposition = Some(RunDisposition::ended(
                    run_id.clone(),
                    EndCause::Error(Failure::StateConflict),
                ));
                break;
            }
            ledger.commit_delta(context, thread_id, run_id).await?;
        }

        if context.is_cancelled() {
            disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::Cancelled));
            break;
        }

        // The environment to drive this step: the live-refreshed one when a
        // dynamic plugin's tool face changed, else the resolved base.
        let env = live_env.current(runtime, resolved, &context.session_plugins, env);

        // RunState hooks stage state (G9/G30). A BeforeInference hook may also inject
        // request-only context (e.g. recalled memories), prepended to this
        // inference and never committed.
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepStart,
            &ledger.transcript,
            &run_input,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::BeforeInference,
            &ledger.transcript,
            &run_input,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;
        // Request-only context the `BeforeInference` hooks wrote to state: the
        // kernel reads it here (single chokepoint) and prepends the flattened
        // per-producer blocks to this inference, never committing them (ADR-0055).
        let prelude: Vec<Message> = ContextMessages::load(&store)
            .map_err(|error| Error::Execution(error.to_string()))?
            .into_values()
            .flatten()
            .collect();
        let context_window = ContextWindow::load(&store)
            .map_err(|error| Error::Execution(error.to_string()))?
            .keep_last_at(ledger.transcript.len());

        // A persisted partial resumes only this drive's first step (see above);
        // it is consumed on the first inference call of that step.
        let step_resume = if step == 0 {
            resume_checkpoint.take()
        } else {
            None
        };
        let checkpoint_ref = checkpoint_ctx.as_ref();
        let final_step = resolved.spec.max_steps > 1 && step + 1 == resolved.spec.max_steps;
        let inference = infer_step(
            &llm,
            runtime,
            resolved,
            env,
            run_id,
            thread_id,
            step_base,
            step,
            &mut ledger,
            &mut store,
            &prelude,
            &discovery,
            checkpoint_ref,
            step_resume,
            context,
            context_window,
            final_step,
        );
        // A cancel aborts a hung or long inference in flight rather than
        // awaiting for the step boundary. Dropping the inference future drops
        // its generation-bound circuit permit, which safely reopens an active
        // half-open probe without a separate check-then-record race.
        let inference = match &context.cancellation {
            Some(token) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    result = inference => Some(result),
                }
            }
            None => Some(inference.await),
        };
        let Some(inference) = inference else {
            disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::Cancelled));
            break;
        };
        let (response, response_model_ref) = match inference {
            Ok(InferStepOutcome::Response {
                response,
                model_ref,
            }) => {
                consecutive_inference_failures = 0;
                (response, model_ref)
            }
            Ok(InferStepOutcome::Await(reason)) => {
                disposition = Some(RunDisposition::awaiting(pause_ticket(
                    context,
                    resolved,
                    run_id,
                    delegation_origin,
                    reason,
                )));
                emit(
                    context,
                    run_id,
                    AgentEvent::Fact(Fact::Awaiting {
                        pending_tool_use_id: None,
                    }),
                )
                .await;
                break;
            }
            Err(err) => {
                // Below the tolerance the failed step is absorbed and the next
                // step re-infers; each absorbed failure still consumes a step,
                // so `max_steps` stays the runaway backstop. At the tolerance
                // the streak ends the run with the last error's classification.
                consecutive_inference_failures += 1;
                if consecutive_inference_failures < runtime.max_consecutive_inference_failures() {
                    continue;
                }
                disposition = Some(RunDisposition::ended(
                    run_id.clone(),
                    EndCause::Error(Failure::Inference {
                        code: err.code().to_string(),
                        message: err.to_string(),
                    }),
                ));
                break;
            }
        };

        // Record this step's token usage as committed thread truth, attributed to the
        // bound model and accumulated across steps and runs, so an adapter can surface
        // a session's usage (total or per-model) by reading thread state — the runtime
        // records the fact without naming any wire, and it survives a restart. A step
        // whose provider reported no usage records nothing.
        if let Some(step_usage) = response.usage {
            fold_thread_usage(
                &mut store,
                &mut ledger.staged_state,
                "thread usage state drifted; skipping record",
                |usage| usage.record(&response_model_ref, step_usage),
            );
        }

        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::AfterInference,
            &ledger.transcript,
            &run_input,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;

        // A cancel may have landed while inference was in flight; observe it at
        // this step boundary and discard the model output.
        if context.is_cancelled() {
            disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::Cancelled));
            break;
        }

        // Commit the assistant step verbatim — text and tool-use blocks may
        // interleave. The text already streamed to the live sink; the committed
        // message is the assembled whole.
        // Reverse the model-facing tool id back to its canonical id (ADR-0053) at the
        // single ingress, BEFORE any gate / delegation / dispatch / audit — so an alias
        // (or an MCP-tool alias) never leaks past this boundary; every internal consumer
        // sees the canonical id. Identity when the agent has no tool presentation.
        let calls: Vec<ToolCall> = response
            .output
            .tool_calls()
            .into_iter()
            .map(|mut call| {
                call.tool_id = resolved
                    .spec
                    .tool_presentation
                    .resolve(&call.tool_id)
                    .to_string();
                call
            })
            .collect();
        let assistant = assistant_message(run_id, step_base + step, response.output.blocks);
        ledger.push_message(assistant);

        // A non-tool MaxTokens response reaches this boundary when its in-place
        // continuation budget is exhausted (including a configured zero budget)
        // or when it contains no continuable text. Preserve any last partial in
        // the ordinary terminal ThreadCommit, but never misreport incomplete
        // output as NaturalEnd.
        if response.stop_reason == Some(StopReason::MaxTokens) && calls.is_empty() {
            disposition = Some(RunDisposition::ended(
                run_id.clone(),
                EndCause::Error(Failure::Inference {
                    code: "max_tokens_exhausted".to_string(),
                    message: "model output remained truncated after the continuation budget was exhausted"
                        .to_string(),
                }),
            ));
            break;
        }

        // A non-compliant model can still emit a tool call even though the
        // reserved final request advertised no tools. Never execute that call:
        // the hard ceiling remains authoritative and the run ends MaxSteps.
        if final_step && !calls.is_empty() {
            disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::MaxSteps));
            break;
        }

        // A text-only step (no tool requests) is a natural end — unless queued
        // live input or a run-end guard keeps the loop going. Queued input is
        // drained first: a message the caller already addressed to this run
        // preempts any guard's end-of-run verdict, exactly as if it had arrived
        // one step earlier.
        if calls.is_empty() {
            // The safe loop boundary (ADR-0054): drain queued live input, honour an
            // operator pause, else fall through to the run-end guard. Shared with
            // every executor via the kernel `evaluate_boundary`.
            match evaluate_boundary(context, run_id, &ledger.transcript) {
                BoundaryOutcome::Continue { fold } => {
                    for message in fold {
                        ledger.push_message(message);
                    }
                    continue;
                }
                BoundaryOutcome::Await { fold, reason } => {
                    // Commit the drained input first (so an in-flight steer is not
                    // lost), then await on a no-tool ticket — an operator pause,
                    // resumed by an explicit resume, not a tool result.
                    for message in fold {
                        ledger.push_message(message);
                    }
                    disposition = Some(RunDisposition::awaiting(pause_ticket(
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        reason,
                    )));
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: None,
                        }),
                    )
                    .await;
                    break;
                }
                BoundaryOutcome::Idle => {}
            }
            // The runtime owns *when* the loop stops; a guard supplies the
            // predicate and any feedback. Its `detail` is opaque
            // (anti-corruption): the kernel forwards it, never interprets it.
            match consult_run_end(
                env,
                run_id,
                &ledger.transcript,
                forced_continuations,
                context.cancellation.as_ref(),
                &store,
            )
            .await
            {
                RunEndOutcome::Steer { feedback, detail } => {
                    // Live progress (best-effort) and durable truth (committed with
                    // the run): the round is both streamed and recorded, so the
                    // round history survives a crash/resume (G1/G13).
                    ledger.audit.push(continuation_event(&detail));
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Continuation {
                            steered: true,
                            detail,
                        }),
                    )
                    .await;
                    let message = feedback_message(run_id, forced_continuations, feedback);
                    ledger.push_message(message);
                    forced_continuations += 1;
                    continue;
                }
                RunEndOutcome::Complete { detail } => {
                    ledger.audit.push(continuation_event(&detail));
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Continuation {
                            steered: false,
                            detail,
                        }),
                    )
                    .await;
                    disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::NaturalEnd));
                    break;
                }
                RunEndOutcome::End => {
                    disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::NaturalEnd));
                    break;
                }
            }
        }

        // Otherwise run each requested tool and feed the results back. A call that
        // awaits or fails the run returns its disposition here, ending the step
        // loop; every call answered with a result returns `None` and the loop goes on.
        let tool_execution = dispatch::run_tool_calls(
            runtime,
            context,
            delegation_origin,
            resolved,
            env,
            run_id,
            thread_id,
            step,
            calls,
            &mut ledger,
            &mut store,
            &mut discovery,
        );
        // Cancellation must also preempt an executing tool. Waiting until the
        // next step boundary leaves a hung shell, package download, or nested
        // container alive after the durable Run has already ended Cancelled.
        // Dropping this future propagates through the tool SPI; process-backed
        // tools use that drop to terminate their complete process group.
        let tool_execution = match &context.cancellation {
            Some(token) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    result = tool_execution => Some(result),
                }
            }
            None => Some(tool_execution.await),
        };
        let Some(tool_execution) = tool_execution else {
            disposition = Some(RunDisposition::ended(run_id.clone(), EndCause::Cancelled));
            break;
        };
        match tool_execution {
            Ok(Some(reached)) => {
                disposition = Some(reached);
                break;
            }
            Ok(None) => {}
            Err(Error::StateConflict) => {
                disposition = Some(RunDisposition::ended(
                    run_id.clone(),
                    EndCause::Error(Failure::StateConflict),
                ));
                break;
            }
            Err(error) => return Err(error),
        }

        // StepEnd fires at the boundary between steps that continue the loop.
        run_phase_hooks(
            env,
            run_id,
            step,
            PhaseHookPoint::StepEnd,
            &ledger.transcript,
            &run_input,
            &mut store,
            &mut ledger.staged_state,
        )
        .await;
    }

    // No early end means the loop exhausted its step ceiling.
    let disposition =
        disposition.unwrap_or_else(|| RunDisposition::ended(run_id.clone(), EndCause::MaxSteps));
    Ok(ledger.into_step_result(disposition))
}

/// Run every hook registered for one phase point, in dependency order, staging
/// their state commands. Hooks return data; they never write a store (G9/G30).
/// Run every phase hook registered for `point`, staging their state commands and
/// collecting their request-only context messages. The context is meaningful only
/// at `BeforeInference` (where the caller prepends it to the request); other points
/// discard it.
#[allow(clippy::too_many_arguments)]
async fn run_phase_hooks(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step: usize,
    point: PhaseHookPoint,
    conversation: &[Message],
    run_input: &std::sync::Arc<[Message]>,
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) {
    // The tool-result phase carries per-call data and runs via
    // `collect_tool_reactions`, never here.
    let kind = match point {
        PhaseHookPoint::StepStart => PhaseKind::StepStart,
        PhaseHookPoint::BeforeInference => PhaseKind::BeforeInference {
            run_input: run_input.clone(),
        },
        PhaseHookPoint::AfterInference => PhaseKind::AfterInference,
        PhaseHookPoint::StepEnd => PhaseKind::StepEnd,
        PhaseHookPoint::AfterTool => {
            unreachable!("AfterTool phase hooks run via collect_tool_reactions")
        }
    };
    for hook in env.hooks_for(point) {
        let ctx = PhaseContext {
            run_id: run_id.clone(),
            step,
            kind: kind.clone(),
        };
        let reaction = hook.on_phase(&ctx, conversation, store).await;
        // Apply the reaction's state to the live store before the next hook, so a
        // once-per-run hook that gates on its own run-scoped key sees its own
        // earlier write and replays instead of recomputing (ADR-0055) — the same
        // apply-then-stage discipline the tool-outcome path uses. Request-only
        // context is written to `ContextMessages` here; the kernel reads it at
        // request assembly (a phase hook never returns request-only messages).
        for command in &reaction.state {
            store.apply(command);
        }
        staged_state.extend(reaction.state);
    }
}

/// The runtime's view of a run-end consultation, folding the registered guards'
/// decisions: the first guard that steers wins; otherwise the run ends carrying
/// the last guard's completion detail; with no guards at all it just ends.
enum RunEndOutcome {
    End,
    Complete {
        detail: serde_json::Value,
    },
    Steer {
        feedback: String,
        detail: serde_json::Value,
    },
}

/// Consult the run-end continuation guards at a natural-end boundary. Guards are
/// consulted in dependency order; a `Steer` short-circuits (a guard wants another
/// step). Their `detail` is opaque to the runtime (G2 neutrality) — forwarded,
/// never interpreted.
async fn consult_run_end(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    conversation: &[Message],
    forced_continuations: usize,
    cancellation: Option<&tokio_util::sync::CancellationToken>,
    state: &Store,
) -> RunEndOutcome {
    let guards = env.run_end_guards();
    if guards.is_empty() {
        return RunEndOutcome::End;
    }
    let mut completion = None;
    for guard in guards {
        let ctx = RunEndContext {
            run_id: run_id.clone(),
            conversation,
            forced_continuations,
            cancellation,
            state,
        };
        match guard.evaluate(&ctx).await {
            RunEndDecision::Steer { feedback, detail } => {
                return RunEndOutcome::Steer { feedback, detail };
            }
            RunEndDecision::Complete { detail } => completion = Some(detail),
        }
    }
    completion
        .map(|detail| RunEndOutcome::Complete { detail })
        .unwrap_or(RunEndOutcome::End)
}

/// A committed event for one run-end continuation round. The opaque `detail` is
/// forwarded verbatim (the kernel never interprets it, G2); committing it makes
/// the round history durable truth the host projects, not a best-effort stream.
fn continuation_event(detail: &serde_json::Value) -> EventDraft {
    RunEvent::Continuation {
        detail: detail.clone(),
    }
    .into()
}

/// The user message a steered continuation injects before looping. Its id is
/// run-scoped and distinct from the step-based assistant/tool message ids.
fn feedback_message(run_id: &RunId, nth: usize, feedback: String) -> Message {
    Message {
        id: MessageId::steer(run_id, nth),
        role: Role::User,
        content: vec![ContentBlock::text(feedback)],
    }
}

/// A no-tool awaiting ticket for any safe-boundary pause: the Run awaits with no
/// pending tool and no call id, correlated by Run id. The owning authority may
/// resume it explicitly or automatically without fabricating a tool result.
fn pause_ticket(
    context: &RuntimeRunContext,
    resolved: &ResolvedRun,
    run_id: &RunId,
    delegation_origin: Option<&DelegationOrigin>,
    reason: PauseReason,
) -> ResumeTicket {
    ResumeTicket::new(
        &run_id.0,
        run_id.clone(),
        ThreadId(String::new()), // filled in finish via the commit thread id
        &resolved.snapshot_id.0,
        &resolved.spec.catalog_fingerprint.0,
        AwaitTarget::Pause(reason),
    )
    .with_delegation_origin(delegation_origin.cloned())
    .with_data_subject(
        context
            .capture
            .subject
            .as_ref()
            .map(|subject| subject.0.clone()),
    )
}
