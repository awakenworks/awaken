// @formal entrypoint tool.executor_entry_order
// @formal entrypoint tool_result.call_is_consumed_at_most_once
// @formal entrypoint tool_result.resume_receipt_is_atomic_with_ticket_consumption
//! Resume-path orchestration from committed thread truth.
//!
//! This module owns ticket-result folding, approval pre-commit, delegation
//! continuation, and re-entry into the ordinary execution loop.

use super::delegation::is_resolved_advisor_call;
use super::*;
use awaken_agent_contract::audit::permission::PermissionConfirmation;

/// Who continues after the common durable tool phase. This selects a control
/// boundary, not a second executor, permission policy, or recovery algorithm.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeContinuation {
    NativeLoop,
    ToolBatch,
}

/// Select the context that still needs to be committed for one durable resume.
/// Exact committed Messages are replay; a stable id with another payload is a
/// conflict. Running this before environment resolution also preserves accepted
/// System context when the resumed Run terminates at the capability boundary.
pub(super) fn fresh_resume_context(
    committed: &[Message],
    context_messages: Vec<Message>,
) -> Result<Vec<Message>> {
    let mut fresh_context = Vec::with_capacity(context_messages.len());
    for message in context_messages {
        match committed
            .iter()
            .find(|committed| committed.id == message.id)
        {
            Some(committed) if committed == &message => {}
            Some(_) => {
                return Err(Error::Execution(format!(
                    "resume context Message `{}` conflicts with committed truth",
                    message.id.0
                )));
            }
            None => fresh_context.push(message),
        }
    }
    Ok(fresh_context)
}

/// Rebuild the model-request transcript and context, retaining the original
/// committed-message ordinal and request-only projection in one read phase.
fn resumed_model_context(
    resolved: &ResolvedRun,
    run_id: &RunId,
    thread_id: &ThreadId,
    reader: &dyn CommittedThreadView,
    context: &RuntimeRunContext,
    fresh_context: &[Message],
) -> (usize, Vec<Message>, RuntimeRunContext) {
    let committed = reader.committed_messages(thread_id);
    // Cause/effect decision table for resumed assistant ids:
    // R1 no prior assistant for this Run -> start at step 0;
    // R2 prior full steps 0..N -> resume at N+1;
    // R3 truncated partials/other Runs -> ignore them when deriving N.
    // This keeps retry/replay ids stable while preventing two distinct scheduled
    // resumes from both minting the former fixed `resume-step-1000` id.
    let resume_step_base =
        awaken_agent_contract::agent::message::next_assistant_step(&committed, run_id);
    let normalized = normalized_model_input(
        context,
        committed,
        run_id,
        thread_id,
        &resolved.snapshot_id.0,
        &resolved.spec.instructions,
    );
    let mut transcript = normalized.durable_input;
    let projected_context = RuntimeRunContext {
        request_context: normalized.request_context,
        ..context.clone()
    };
    transcript.extend(fresh_context.iter().cloned());
    (resume_step_base, transcript, projected_context)
}

fn permission_confirmation(call: &ToolCall, result: PermissionConfirmation) -> EventDraft {
    RunEvent::PermissionConfirmed {
        tool_id: call.tool_id.clone(),
        call_id: call.call_id.clone(),
        result,
    }
    .into()
}

/// The original atomic approval frame: ticket consumption, Executing state and
/// one confirmation share this ThreadCommit before the admitted tool is invoked.
fn approved_tool_commit(
    ticket: &ResumeTicket,
    call: &ToolCall,
    state: Vec<StateCommand>,
    resume: Option<&ResumeCommit>,
) -> Result<awaken_agent_contract::thread::commit::staged::ThreadCommit> {
    assemble_attempt_commit(
        ticket.thread_id.clone(),
        RunDisposition::running(ticket.run_id.clone()),
        true,
        Vec::new(),
        state,
        vec![permission_confirmation(call, PermissionConfirmation::Allow)],
        resume,
    )
}

/// Rebuild the transcript and state from committed truth, inject a resumed
/// `result` for the awaiting ticket, and drive the loop to its next terminal/awaiting
/// checkpoint. Shared by the two resume entries — a validated `ResumeCommand`
/// (`resume_run`) and a delegate step folded back into the parent
/// (`resume_delegation`) — so the rebuild/inject/drive/finalize glue lives once.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_resumed(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    ticket: &ResumeTicket,
    result: ResumeResult,
    mut resume_commit: Option<ResumeCommit>,
    mut fresh_context: Vec<Message>,
    run_input: std::sync::Arc<[Message]>,
    reader: &dyn CommittedThreadView,
    context: &RuntimeRunContext,
    continuation: ResumeContinuation,
    resume_checkpoint: Option<StreamCheckpoint>,
    mut returned_child: Option<ChildRunResult>,
    mut waiting_batch: Option<ToolBatch>,
    mut store: Store,
) -> Result<RunState> {
    let (resume_step_base, mut transcript, projected_context) =
        resumed_model_context(resolved, run_id, thread_id, reader, context, &fresh_context);
    let context = &projected_context;
    let mut authority_state = Vec::new();
    awaken_runtime_contract::permission::RunToolCapability::stage(
        &mut store,
        &mut authority_state,
        context.tool_capability_narrowing,
        false,
    )
    .map_err(|error| Error::Commit(error.to_string()))?;
    // Keep the state already validated at recovery admission, including model
    // history and any child result committed by resume_delegation. Rebuilding
    // from the reader here would discard both the validated ordinal and local
    // projection updates when the reader is a frozen Worker snapshot.
    // Context and batch were admitted by the common entry before any child
    // effect. Read the original aggregate's call rather than rebuilding another
    // payload from the ticket after a child has already been resumed.
    let pending_call = pending_tool_call(waiting_batch.as_ref(), ticket).cloned();
    let waiting_call_id = pending_call.as_ref().map(|call| call.call_id.clone());
    let approved = matches!(
        &result,
        ResumeResult::Permission(PermissionDecision::Allow { .. })
    );
    let permission_decision = match &result {
        ResumeResult::Permission(decision)
            if matches!(
                ticket.reason(),
                AwaitReason::ToolPermission | AwaitReason::ScheduledAction
            ) =>
        {
            Some(PermissionConfirmation::from(decision))
        }
        _ => None,
    };
    let mut decision_precommitted = false;
    let mut delegation_state = Vec::new();
    // Approval is a lifecycle decision, not a chat message. For a batch-aware
    // ticket, commit Awaiting(Approval) -> Executing before entering the tool.
    // A failed/fenced commit therefore cannot leak an unrecorded side effect.
    if matches!(
        &result,
        ResumeResult::Permission(PermissionDecision::Allow { .. })
    ) && matches!(
        ticket.reason(),
        AwaitReason::ToolPermission | AwaitReason::ScheduledAction
    ) && let Some(call) = pending_call.as_ref()
    {
        let call_id = call.call_id.as_str();
        let batch = waiting_batch.as_mut().ok_or_else(|| {
            Error::Execution("approved tool resume lost its committed tool batch".to_string())
        })?;
        let wait_kind = match ticket.reason() {
            AwaitReason::ToolPermission => ToolWaitKind::ToolPermission,
            AwaitReason::ScheduledAction => ToolWaitKind::ScheduledAction,
            _ => unreachable!("guarded by the resume reason above"),
        };
        let entry = batch
            .resume_executing(call_id, wait_kind, &ticket.correlation_id)
            .map_err(|error| Error::Execution(error.to_string()))?;
        // Exact wait admission comes first. Domain preparation can perform
        // lazy realization; failure leaves the old call and ticket unchanged.
        super::tool_execution::enter_pending_tool(runtime, env, context, call, entry).await?;
        let command = ActiveToolBatch::write(&Some(batch.clone()));
        let mut approval_state = Vec::new();
        stage_delegation_request(
            runtime,
            resolved,
            ticket.delegation_origin.as_ref(),
            run_id,
            call,
            &mut store,
            &mut approval_state,
        )?;
        approval_state.push(command.clone());
        approval_state.extend(authority_state.iter().cloned());
        if let Some(coordinator) = &context.commit {
            coordinator
                .commit(approved_tool_commit(
                    ticket,
                    call,
                    approval_state,
                    resume_commit.as_ref(),
                )?)
                .await
                .map_err(Error::from)?;
            resume_commit = None;
            authority_state.clear();
        }
        store.apply(&command);
        decision_precommitted = true;
    }
    let result = if approved && let Some(call) = pending_call.as_ref() {
        match super::tool_execution::cancellable_tool_phase(
            context,
            invoke_delegation(
                runtime,
                resolved,
                DelegationParent {
                    context,
                    origin: ticket.delegation_origin.as_ref(),
                    agent_id: &resolved.agent_id.0,
                    run_id,
                    thread_id,
                },
                call,
                &store,
            ),
        )
        .await?
        {
            Some(Ok(DelegationInvocation {
                id,
                step: DelegationStep::Ended { text, usage },
            })) => {
                returned_child = Some(ChildRunResult {
                    child_run_id: id.child_run_id(),
                    text: text.clone(),
                    usage,
                });

                ResumeResult::ToolResult(ToolOutput::ok(&call.call_id, text))
            }
            Some(Ok(DelegationInvocation {
                step: DelegationStep::Awaiting { continuation },
                ..
            })) => {
                stage_delegation_awaiting(
                    runtime,
                    run_id,
                    call,
                    &continuation,
                    &mut store,
                    &mut delegation_state,
                )?;
                let next_ticket = resume_ticket(
                    context,
                    resolved,
                    run_id,
                    ticket.delegation_origin.as_ref(),
                    &call.call_id,
                    call,
                    ToolAwaitReason::Delegation,
                );
                let batch = waiting_batch.as_mut().ok_or_else(|| {
                    Error::Execution(
                        "approved delegation is missing its committed tool batch".to_string(),
                    )
                })?;
                batch
                    .mark_awaiting(
                        &call.call_id,
                        ToolWaitKind::Delegation,
                        next_ticket.correlation_id.clone(),
                    )
                    .map_err(|error| Error::Execution(error.to_string()))?;
                delegation_state.push(ActiveToolBatch::write(&Some(batch.clone())));
                return finish(
                    runtime,
                    context,
                    thread_id,
                    run_id.clone(),
                    RunStepResult {
                        new_messages: fresh_context,
                        staged_state: delegation_state,
                        audit: Vec::new(),
                        resume: None,
                        disposition: RunDisposition::awaiting(next_ticket),
                    },
                )
                .await;
            }
            Some(Err(error)) => {
                ResumeResult::ToolResult(delegation_error_output(resolved, call, error)?)
            }
            None if is_resolved_advisor_call(resolved, call) => {
                ResumeResult::ToolResult(ToolOutput::error(
                    &call.call_id,
                    awaken_runtime_contract::resolved::ADVISOR_UNAVAILABLE_NOTICE,
                ))
            }
            None => result,
        }
    } else {
        result
    };
    let execution_admission = if matches!(
        &result,
        ResumeResult::Permission(PermissionDecision::Allow { .. })
    ) {
        let call_id = ticket.call_id().ok_or_else(|| {
            Error::Execution("approved tool resume has no pending call".to_string())
        })?;
        Some(
            waiting_batch
                .as_ref()
                .ok_or_else(|| {
                    Error::Execution(
                        "approved tool resume lost its committed tool batch".to_string(),
                    )
                })?
                .execution_admission(call_id)
                .map_err(|error| Error::Execution(error.to_string()))?,
        )
    } else {
        None
    };
    let run_reactions = !matches!(
        &result,
        ResumeResult::Permission(PermissionDecision::Deny { .. })
    );
    let (resumed, mut seed_state, resumed_output) = resume_into_messages(
        ResumeExecutionContext {
            runtime,
            resolved,
            env,
            context,
            batch: waiting_batch.as_ref(),
            execution_admission,
        },
        run_id,
        ticket,
        result,
        &store,
    )
    .await?;
    seed_state.splice(0..0, delegation_state);
    let mut seed_audit = if decision_precommitted {
        Vec::new()
    } else if let (Some(decision), Some(call)) = (permission_decision, pending_call.as_ref()) {
        vec![permission_confirmation(call, decision)]
    } else {
        Vec::new()
    };
    // Admission state stays outside the tool's raw output and retained envelope.
    seed_state.extend(authority_state);
    for command in &seed_state {
        store.apply(command);
    }
    let mut new_messages = Vec::new();
    if let Some(mut batch) = waiting_batch.take() {
        let call_id = waiting_call_id.as_deref().expect("validated waiting call");
        let output = resumed_output
            .ok_or_else(|| Error::Execution("tool wait resumed without a tool output".into()))?;
        batch
            .record_returned(output.clone())
            .map_err(|error| Error::Execution(error.to_string()))?;
        // Publication appends this exact accepted suffix once, before Tool.
        // It must not already exist in the model transcript as a fresh copy.
        transcript.truncate(transcript.len() - fresh_context.len());
        batch
            .set_returned_context(call_id, std::mem::take(&mut fresh_context))
            .map_err(|error| Error::Execution(error.to_string()))?;
        let call = pending_call.as_ref().expect("validated pending call");
        let mut ledger = StepLedger::new(transcript, Vec::new(), seed_state, seed_audit);
        ledger.attach_resume(resume_commit.take());
        let recorded = dispatch::record_tool_output(
            runtime,
            env,
            run_id,
            thread_id,
            context,
            resume_step_base,
            call,
            run_reactions.then_some(call),
            output,
            returned_child,
            false,
            approved || ticket.reason() == AwaitReason::Delegation,
            dispatch::BatchPersistence::Always,
            &mut batch,
            &mut ledger,
            &mut store,
        )
        .await;
        if let Err(error) = recorded {
            if super::tool_execution::is_tool_phase_cancelled(&error) {
                ledger
                    .staged_state
                    .push(ActiveToolBatch::write(&Some(batch)));
                return super::finalize::finish_with_reader(
                    runtime,
                    context,
                    Some(reader),
                    thread_id,
                    run_id.clone(),
                    ledger.into_step_result(RunDisposition::ended(
                        run_id.clone(),
                        EndCause::Cancelled,
                    )),
                )
                .await;
            }
            return Err(error);
        }
        transcript = ledger.transcript.clone();
        let tail = ledger.into_step_result(RunDisposition::running(run_id.clone()));
        seed_state = tail.staged_state;
        seed_audit = tail.audit;
        resume_commit = tail.resume;
        new_messages = tail.new_messages;
    } else {
        transcript.extend(resumed.iter().cloned());
        new_messages.extend(fresh_context);
        new_messages.extend(resumed);
    }
    if continuation == ResumeContinuation::ToolBatch {
        let mut ledger = StepLedger::new(transcript, new_messages, seed_state, seed_audit);
        ledger.attach_resume(resume_commit);
        return drive_tool_batch(
            runtime,
            resolved,
            env,
            run_id,
            thread_id,
            ticket.delegation_origin.as_ref(),
            context,
            ledger,
            store,
        )
        .await;
    }
    let step_result = drive(
        runtime,
        resolved,
        env,
        run_id,
        thread_id,
        context,
        ticket.delegation_origin.as_ref(),
        transcript,
        new_messages,
        run_input,
        resume_step_base,
        store,
        seed_state,
        seed_audit,
        resume_commit,
        resume_checkpoint,
    )
    .await?;
    finalize(runtime, context, thread_id, run_id.clone(), step_result).await
}

/// Shared permission-resume/reclaim handoff. The ledger and batch remain the
/// only result authority; no Native inference or terminal fact is invented for
/// a completed tool phase.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_tool_batch(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    delegation_origin: Option<&DelegationOrigin>,
    context: &RuntimeRunContext,
    mut ledger: StepLedger,
    mut store: Store,
) -> Result<RunState> {
    let mut discovery =
        ToolDiscoveryStateKey::load(&store).map_err(|error| Error::Execution(error.to_string()))?;
    if let Some(disposition) = dispatch::recover_pending_tool_batch(
        runtime,
        context,
        delegation_origin,
        resolved,
        env,
        run_id,
        thread_id,
        &mut ledger,
        &mut store,
        &mut discovery,
    )
    .await?
    {
        return finalize(
            runtime,
            context,
            thread_id,
            run_id.clone(),
            ledger.into_step_result(disposition),
        )
        .await;
    }
    if ledger.has_uncommitted() {
        ledger.commit_delta(context, thread_id, run_id).await?;
    }
    Ok(RunState::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    #[test]
    fn resume_context_replay_is_exact_or_conflicting() {
        // Cause/effect graph: C1 a stable context id is absent, committed with
        // the exact payload, or committed with another payload. Effects: E1 an
        // absent Message remains fresh; E2 exact replay produces no second
        // Message; E3 a different payload fails before Runtime commits anything.
        //
        // | Rule | committed same id | payload | Effect |
        // |---|---|---|---|
        // | R1 | no | - | E1 fresh |
        // | R2 | yes | exact | E2 no-op |
        // | R3 | yes | changed | E3 conflict |
        // Constraints/invariants: stable Message id+payload is one idempotency
        // coordinate; replay cannot duplicate or overwrite committed context.
        let exact = Message::text(
            MessageId("session-system-reply".into()),
            Role::System,
            "context",
        );
        assert_eq!(
            fresh_resume_context(&[], vec![exact.clone()]).expect("R1"),
            vec![exact.clone()],
            "R1/E1"
        );
        assert!(
            fresh_resume_context(std::slice::from_ref(&exact), vec![exact.clone()])
                .expect("R2")
                .is_empty(),
            "R2/E2"
        );
        let changed = Message::text(exact.id.clone(), Role::System, "changed");
        assert!(
            fresh_resume_context(std::slice::from_ref(&exact), vec![changed]).is_err(),
            "R3/E3"
        );
    }
}
