// @formal entrypoint delegation.remote_cancel_redelivery
// @formal entrypoint remote_attempt.cancel_addresses_pinned_task
// @formal entrypoint remote_attempt.committed_task_is_endpoint_pinned
// @formal entrypoint remote_attempt.endpoint_mismatch_fails_closed
// @formal entrypoint remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay
// @formal entrypoint remote_attempt.recovery_reattaches_without_resending
// @formal entrypoint remote_attempt.replay_uses_stable_message_identity
// @formal entrypoint remote_attempt.resume_preserves_context_and_task_reference
// @formal entrypoint session.legacy_derived_context_remains_durable_but_is_hidden_from_new_model_requests
//! Execution of one publication-pinned remote A2A attempt.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_runtime_contract::diagnostic::{RuntimeDiagnostic, RuntimeErrorKind};

pub(super) fn ensure_supported_narrowing(
    activation: &RunActivation,
    context: &RuntimeRunContext,
) -> Result<()> {
    if !activation.tool_capability_narrowing().is_configured()
        || !context.tool_capability_narrowing.is_configured()
        || context.tool_permission_policy.is_some()
    {
        return Err(Error::Execution(
            "A2A cannot prove enforcement of this Run's deny-all tool capability".to_string(),
        ));
    }
    Ok(())
}

fn preserve_remote_uncertainty<T>(context: &RuntimeRunContext, error: Error) -> Result<T> {
    context.physical_quiescence.mark_uncertain();
    Err(error)
}

fn reject_unconfirmed_operation<T>(
    context: &RuntimeRunContext,
    intent: &RemoteOperationIntent,
    action: &str,
) -> Result<T> {
    let cause = Error::Execution(format!(
        "A2A operation {:?} (context {:?}, message {:?}) has no committed remote task; \
         automatic {action} is unsafe. Inspect the original operation on the peer; \
         keep this Run blocked until its outcome is reconciled",
        intent.operation_id, intent.context_id, intent.message_id
    ));
    preserve_remote_uncertainty(
        context,
        RuntimeDiagnostic::new(
            RuntimeErrorKind::Conflict,
            "a2a_operation_recovery_unconfirmed",
            cause,
        )
        .into(),
    )
}

async fn commit_send_accept_remote_operation(
    transport: Arc<dyn Transport>,
    context: &RuntimeRunContext,
    activation: &RunActivation,
    intent: RemoteOperationIntent,
    text: &str,
    prepared_messages: Vec<awaken_agent_contract::agent::message::Message>,
) -> Result<(AcceptedRemoteOperation, awaken_protocol_a2a::Task)> {
    let mut phase = RemoteOperationSequencePhase::Fresh;

    let next = remote_operation_sequence_step(phase, RemoteOperationSequenceEvent::CommitPrepared)
        .ok_or_else(|| Error::Execution("A2A Prepared commit is out of sequence".into()))?;
    verify_attempt_ownership(context.ownership.as_deref()).await?;
    commit_boundary(
        context,
        activation,
        RunDisposition::running(activation.run_id.clone()),
        prepared_messages,
        vec![prepared_operation_state(&intent)?],
    )
    .await?;
    phase = next;
    task_driver::observe_remote_operation(context, async {
        let next = remote_operation_sequence_step(phase, RemoteOperationSequenceEvent::Send)
            .ok_or_else(|| Error::Execution("A2A send is out of sequence".into()))?;
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let task = send_message(
            transport.as_ref(),
            None,
            &intent.context_id,
            &intent.message_id,
            text,
        )
        .await
        .map_err(|error| Error::Execution(error.to_string()))?;
        phase = next;

        let accepted = accept_remote_operation(intent, &task)?;
        let next =
            remote_operation_sequence_step(phase, RemoteOperationSequenceEvent::CommitAccepted)
                .ok_or_else(|| Error::Execution("A2A Accepted commit is out of sequence".into()))?;
        commit_boundary(
            context,
            activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![accepted_operation_state(&accepted)?],
        )
        .await?;
        debug_assert_eq!(next, RemoteOperationSequencePhase::AcceptedCommitted);
        let task =
            task_driver::poll_to_boundary(transport, &accepted.reference, task, context).await?;
        Ok((accepted, task))
    })
    .await
}

#[async_trait]
impl RunExecutor for A2aRunExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        ExecutorCapabilities {
            cancellation: Cancellation::RemoteAbort,
            wait: Wait::Both,
            live_input: awaken_runtime_contract::execution::LiveInput::None,
        }
    }

    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        ensure_supported_narrowing(&activation, &context)?;
        let Ok(candidate) = remote_candidate_of(&activation) else {
            // Reached without a remote backend — a wiring fault; fail closed.
            let mut messages = activation.input.clone();
            messages.push(assistant_message(
                &context,
                &activation,
                "backend is not an A2A endpoint",
            ));
            return finish_terminal(
                &context,
                &activation,
                messages,
                EndCause::Error(Failure::Inference {
                    code: "a2a_config".to_string(),
                    message: "backend is not a2a".to_string(),
                }),
            )
            .await;
        };
        let endpoint = endpoint_of(candidate)?;

        let route = match restored_remote_operation_route(&context, &activation, &endpoint) {
            Ok(route) => route,
            Err(error) => {
                return preserve_remote_uncertainty(&context, error);
            }
        };
        let (accepted, task) = match route {
            RemoteOperationRoute::Prepared(intent) => {
                // The commit/send gap is deliberately not replayed. Neither the
                // stable message id nor a predecessor task can prove whether
                // this exact operation was accepted by the peer.
                return reject_unconfirmed_operation(&context, &intent, "resend");
            }
            RemoteOperationRoute::Reattach(accepted) => {
                task_driver::observe_remote_operation(&context, async {
                    verify_attempt_ownership(context.ownership.as_deref()).await?;
                    let transport = self
                        .transport_resolver
                        .resolve(candidate, &context)
                        .await
                        .map_err(Error::Execution)?;
                    verify_attempt_ownership(context.ownership.as_deref()).await?;
                    let task = get_task(transport.as_ref(), &accepted.reference.task_id)
                        .await
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    validate_reattached_task(&accepted.reference, &task)?;
                    let task = task_driver::poll_to_boundary(
                        transport,
                        &accepted.reference,
                        task,
                        &context,
                    )
                    .await?;
                    Ok((accepted, task))
                })
                .await?
            }
            RemoteOperationRoute::Fresh => {
                verify_attempt_ownership(context.ownership.as_deref()).await?;
                let transport = self
                    .transport_resolver
                    .resolve(candidate, &context)
                    .await
                    .map_err(Error::Execution)?;
                let normalized = awaken_runtime_contract::NormalizedModelInput::new(
                    &activation.snapshot.resolved_spec.instructions,
                    &context.request_context,
                    &activation.input,
                );
                let trace = normalized.trace();
                tracing::debug!(
                    run_id = %activation.run_id.0,
                    thread_id = %activation.thread_id.0,
                    publication_id = %activation.snapshot.fingerprint.0,
                    backend = "a2a",
                    has_instructions = trace.has_instructions,
                    request_context_supplied = trace.request_context_supplied,
                    request_context_visible = trace.request_context_visible,
                    durable_input_supplied = trace.durable_input_supplied,
                    durable_input_visible = trace.durable_input_visible,
                    legacy_derived_filtered = trace.legacy_derived_filtered,
                    "projected privacy-safe model input"
                );
                let prompt = normalized.text_envelope();
                let message_id = stable_message_id(&activation.run_id, None)?;
                let intent = prepare_remote_operation(
                    &endpoint,
                    &activation.thread_id.0,
                    None,
                    &message_id,
                    &prompt,
                )?;
                commit_send_accept_remote_operation(
                    transport,
                    &context,
                    &activation,
                    intent,
                    &prompt,
                    activation.input.clone(),
                )
                .await?
            }
        };
        task_driver::finish_task(&activation, &context, accepted, task).await
    }
}

#[async_trait]
impl RunAttemptExecutor for A2aRunExecutor {
    fn physical_attempt_recovery(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
    ) -> AttemptRecoveryMode {
        // Cause C1: the recovered Run has a complete committed A2A Accepted state.
        // Cause C2: its task reference is bound to the publication-frozen endpoint.
        // Cause C3: the state is only Prepared, so send acceptance is unknown.
        // Effect E1: C1+C2 permits replacing only the Dispatch physical slot;
        // `execute` then uses `tasks/get` and cannot repeat `message:send`.
        // Effect E2: every absent, malformed, non-remote, mismatched, or C3 state
        // retains the generic predecessor-quiescence fence.
        //
        // | Rule | C1 | C2 | C3 | Recovery mode |
        // |---|---|---|---|---|
        // | R1 | yes | yes | no | continue durable remote operation |
        // | R2 | no | n/a | no | require predecessor quiescence |
        // | R3 | yes | no | no | require predecessor quiescence |
        // | R4 | no | n/a | yes | require predecessor quiescence |
        let continuation_is_bound = (|| {
            let candidate = remote_candidate_of(activation).ok()?;
            let endpoint = endpoint_of(candidate).ok()?;
            matches!(
                restored_remote_operation_route(context, activation, &endpoint).ok()?,
                RemoteOperationRoute::Reattach(_)
            )
            .then_some(())
        })()
        .is_some();
        if continuation_is_bound {
            AttemptRecoveryMode::ContinueDurableRemoteOperation
        } else {
            AttemptRecoveryMode::RequirePredecessorQuiescence
        }
    }

    async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        ensure_supported_narrowing(&activation, &context)?;
        let reader = context
            .reader
            .as_ref()
            .ok_or_else(|| Error::Execution("A2A resume requires committed history".to_string()))?;
        let ticket = reader
            .resume_ticket(&activation.run_id)
            .ok_or_else(|| Error::Execution("A2A run is not awaiting a resume".to_string()))?;
        validate_resume(&ticket, &command)
            .map_err(|error| Error::Execution(format!("invalid A2A resume: {error}")))?;
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let accepted = match restored_remote_operation_route(&context, &activation, &endpoint) {
            Ok(RemoteOperationRoute::Reattach(accepted)) => accepted,
            Ok(RemoteOperationRoute::Fresh) => {
                return finish_missing_remote_operation(
                    &context,
                    &activation,
                    Error::Execution("A2A resume is missing its durable remote task".to_string()),
                )
                .await;
            }
            Ok(RemoteOperationRoute::Prepared(intent)) => {
                return reject_unconfirmed_operation(&context, &intent, "resume");
            }
            Err(error) => {
                return preserve_remote_uncertainty(&context, error);
            }
        };
        let reference = &accepted.reference;
        let text = resume_text(&command.result)?;
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let transport = self
            .transport_resolver
            .resolve(candidate, &context)
            .await
            .map_err(Error::Execution)?;
        let message_id = stable_message_id(&activation.run_id, Some(&ticket.correlation_id))?;
        let intent = prepare_remote_operation(
            &endpoint,
            &reference.context_id,
            Some(&reference.task_id),
            &message_id,
            &text,
        )?;
        let (accepted, task) = commit_send_accept_remote_operation(
            transport,
            &context,
            &activation,
            intent,
            &text,
            Vec::new(),
        )
        .await?;
        task_driver::finish_task(&activation, &context, accepted, task).await
    }

    async fn cancel(&self, activation: RunActivation, context: RuntimeRunContext) -> Result<()> {
        let candidate = remote_candidate_of(&activation)?;
        let endpoint = endpoint_of(candidate)?;
        let reference = match restored_remote_operation_route(&context, &activation, &endpoint) {
            Ok(RemoteOperationRoute::Fresh) => return Ok(()),
            Ok(RemoteOperationRoute::Prepared(intent)) => {
                return reject_unconfirmed_operation(&context, &intent, "cancellation");
            }
            Ok(RemoteOperationRoute::Reattach(accepted)) => accepted.reference,
            Err(error) => {
                return preserve_remote_uncertainty(&context, error);
            }
        };
        task_driver::observe_remote_operation(&context, async {
            verify_attempt_ownership(context.ownership.as_deref()).await?;
            let transport = self
                .transport_resolver
                .resolve(candidate, &context)
                .await
                .map_err(Error::Execution)?;
            verify_attempt_ownership(context.ownership.as_deref()).await?;
            let task = get_task(transport.as_ref(), &reference.task_id)
                .await
                .map_err(|error| Error::Execution(error.to_string()))?;
            validate_reattached_task(&reference, &task)?;
            if task.status.state.is_terminal() {
                return Ok(());
            }
            task_driver::cancel_and_observe(transport.as_ref(), &reference, &context).await?;
            Ok(())
        })
        .await
    }
}
